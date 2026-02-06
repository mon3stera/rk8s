use std::collections::HashMap;

use etcd_client::{Client, Compare, CompareOp, Txn, TxnOp, TxnOpResponse, TxnResponse};

use crate::meta::backoff::backoff;
use crate::meta::store::MetaError;

#[allow(dead_code)]
enum UpdateAction {
    Write(Vec<u8>),
    Delete,
    Skip,
}

pub(crate) struct UpdatePlan {
    key: String,
    compare: Compare,
    action: UpdateAction,
}

impl UpdatePlan {
    pub(crate) fn new_write(
        ctx: &TxnContext,
        key: impl Into<String>,
        value: Vec<u8>,
    ) -> Result<Self, MetaError> {
        let key = key.into();
        let compare = ctx.compare_for(&key)?;

        Ok(Self {
            key,
            compare,
            action: UpdateAction::Write(value),
        })
    }

    pub(crate) fn new_delete(ctx: &TxnContext, key: impl Into<String>) -> Result<Self, MetaError> {
        let key = key.into();
        let compare = ctx.compare_for(&key)?;

        Ok(Self {
            key,
            compare,
            action: UpdateAction::Delete,
        })
    }
}

pub(crate) struct TxnEntry {
    value: Option<Vec<u8>>,
    mod_revision: i64,
}

pub(crate) struct TxnContext {
    pub(crate) slots: HashMap<String, TxnEntry>,
}

impl TxnContext {
    pub(crate) fn compare_for(&self, key: &str) -> Result<Compare, MetaError> {
        let Some(entry) = self.slots.get(key) else {
            return Err(MetaError::Internal(format!(
                "Missing key in transaction context: {key}"
            )));
        };

        if entry.mod_revision == 0 {
            Ok(Compare::version(key, CompareOp::Equal, 0))
        } else {
            Ok(Compare::mod_revision(
                key,
                CompareOp::Equal,
                entry.mod_revision,
            ))
        }
    }

    pub(crate) fn value(&self, key: &str) -> Option<&[u8]> {
        self.slots.get(key).and_then(|entry| entry.value.as_deref())
    }

    #[cfg(feature = "rkyv-serialization")]
    pub(crate) fn value_deserialized<T>(&self, key: &str) -> Result<Option<T>, MetaError>
    where
        T: rkyv::Archive,
        T::Archived:
            rkyv::Deserialize<T, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>>,
        for<'de> T: serde::Deserialize<'de>,
    {
        self.slots
            .get(key)
            .and_then(|entry| {
                entry
                    .value
                    .as_deref()
                    .map(|e| crate::meta::serialization::deserialize_meta(e))
            })
            .transpose()
    }

    #[cfg(not(feature = "rkyv-serialization"))]
    pub(crate) fn value_deserialized<T>(&self, key: &str) -> Result<Option<T>, MetaError>
    where
        for<'de> T: serde::Deserialize<'de>,
    {
        self.slots
            .get(key)
            .and_then(|entry| {
                entry
                    .value
                    .as_deref()
                    .map(|e| crate::meta::serialization::deserialize_meta(e))
            })
            .transpose()
    }

    async fn fetch(client: &mut Client, keys: &[String]) -> Result<Self, MetaError> {
        if keys.is_empty() {
            return Ok(Self {
                slots: HashMap::new(),
            });
        }

        let ops: Vec<TxnOp> = keys
            .iter()
            .map(|key| TxnOp::get(key.as_bytes(), None))
            .collect();

        let txn = Txn::new().and_then(ops);
        let resp = client
            .txn(txn)
            .await
            .map_err(|e| MetaError::Internal(format!("Etcd txn fetch error: {e}")))?;

        let mut slots = HashMap::with_capacity(keys.len());

        // Etcd preserves response order for each request op in the txn success list.
        let responses = resp.op_responses();

        for (idx, key) in keys.iter().enumerate() {
            let entry = match responses.get(idx) {
                Some(TxnOpResponse::Get(range_resp)) => range_resp
                    .kvs()
                    .first()
                    .map(|kv| TxnEntry {
                        value: Some(kv.value().to_vec()),
                        mod_revision: kv.mod_revision(),
                    })
                    .unwrap_or(TxnEntry {
                        value: None,
                        mod_revision: 0,
                    }),
                Some(_) => {
                    return Err(MetaError::Internal(format!(
                        "Unexpected txn response for key {key}"
                    )));
                }
                None => {
                    return Err(MetaError::Internal(format!(
                        "Missing txn response for key {key}"
                    )));
                }
            };

            slots.insert(key.clone(), entry);
        }

        Ok(Self { slots })
    }
}

fn build_ops(
    ctx: &TxnContext,
    plans: Vec<UpdatePlan>,
) -> Result<(Vec<Compare>, Vec<TxnOp>), MetaError> {
    let mut compares = Vec::new();
    let mut ops = Vec::new();
    let mut seen_keys = std::collections::HashSet::new();

    for plan in plans {
        if !ctx.slots.contains_key(&plan.key) {
            return Err(MetaError::Internal(format!(
                "Stage generated plan for undeclared key: {}",
                plan.key
            )));
        }

        if !seen_keys.insert(plan.key.clone()) {
            return Err(MetaError::Internal(format!(
                "Duplicate update plan for key {}",
                plan.key
            )));
        }

        match plan.action {
            UpdateAction::Skip => continue,
            UpdateAction::Write(value) => {
                compares.push(plan.compare);
                ops.push(TxnOp::put(plan.key, value, None));
            }
            UpdateAction::Delete => {
                compares.push(plan.compare);
                ops.push(TxnOp::delete(plan.key, None));
            }
        }
    }

    Ok((compares, ops))
}

async fn execute_stage<R, F>(
    client: &Client,
    deps: Vec<String>,
    build: &F,
    retry_on_conflict: bool,
) -> Result<(R, Option<TxnResponse>), MetaError>
where
    R: Default,
    F: Fn(&TxnContext) -> Result<(Vec<UpdatePlan>, R), MetaError> + Send + Sync,
{
    let mut client = client.clone();

    let ctx = TxnContext::fetch(&mut client, &deps).await?;
    let (plans, result) = build(&ctx)?;

    if plans.is_empty() {
        return Ok((R::default(), None));
    }

    let (compares, ops) = build_ops(&ctx, plans)?;

    if ops.is_empty() {
        return Ok((R::default(), None));
    }

    let txn = Txn::new().when(compares).and_then(ops);

    match client.txn(txn).await {
        Ok(resp) if resp.succeeded() => Ok((result, Some(resp))),
        Ok(_) => {
            if retry_on_conflict {
                Err(MetaError::ContinueRetry)
            } else {
                Err(MetaError::TxnConflict)
            }
        }
        Err(e) => Err(MetaError::Internal(format!(
            "Failed to execute transaction: {e}"
        ))),
    }
}

pub(crate) async fn execute_backoff_with<R, F>(
    client: &Client,
    deps: Vec<String>,
    max_retries: u64,
    build: F,
) -> Result<(R, Option<TxnResponse>), MetaError>
where
    R: Default,
    F: Fn(&TxnContext) -> Result<(Vec<UpdatePlan>, R), MetaError> + Send + Sync,
{
    let client = client.clone();

    let attempt = || {
        let deps = deps.clone();
        let client = client.clone();
        let build = &build;

        async move { execute_stage(&client, deps, build, true).await }
    };

    backoff(max_retries, attempt).await
}

pub(crate) async fn execute_once_with<R, F>(
    client: &Client,
    deps: Vec<String>,
    build: F,
) -> Result<(R, Option<TxnResponse>), MetaError>
where
    R: Default,
    F: Fn(&TxnContext) -> Result<(Vec<UpdatePlan>, R), MetaError> + Send + Sync,
{
    execute_stage(client, deps, &build, false).await
}

pub(crate) async fn execute_backoff<F>(
    client: &Client,
    deps: Vec<String>,
    max_retries: u64,
    build: F,
) -> Result<Option<TxnResponse>, MetaError>
where
    F: Fn(&TxnContext) -> Result<Vec<UpdatePlan>, MetaError> + Send + Sync,
{
    let ((), resp) = execute_backoff_with(client, deps, max_retries, |ctx| {
        let plans = build(ctx)?;
        Ok((plans, ()))
    })
    .await?;
    Ok(resp)
}

pub(crate) async fn execute_once<F>(
    client: &Client,
    deps: Vec<String>,
    build: F,
) -> Result<Option<TxnResponse>, MetaError>
where
    F: Fn(&TxnContext) -> Result<Vec<UpdatePlan>, MetaError> + Send + Sync,
{
    let ((), resp) = execute_once_with(client, deps, |ctx| {
        let plans = build(ctx)?;
        Ok((plans, ()))
    })
    .await?;
    Ok(resp)
}
