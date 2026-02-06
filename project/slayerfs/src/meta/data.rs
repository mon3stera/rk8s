use crate::chuck::reader::DataFetcher;
use crate::chuck::slice::block_span_iter;
use crate::chuck::writer::DataUploader;
use crate::chuck::{BlockStore, ChunkLayout, SliceDesc};
use crate::meta::store::MetaError;
use crate::utils::NumCastExt;
use crate::utils::zero::make_zero_bytes;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait WithData {
    async fn write_compact_slice(
        &self,
        new_id: u64,
        offset: u64,
        length: u64,
        old: &[SliceDesc],
    ) -> Result<SliceDesc, MetaError>;

    async fn delete_slices(&self, slices: &[SliceDesc]) -> Result<(), MetaError>;
}

type WriteCompactSliceFn = Box<
    dyn for<'a> Fn(u64, u64, u64, &'a [SliceDesc]) -> BoxFuture<'a, Result<SliceDesc, MetaError>>
        + Send
        + Sync,
>;

type DeleteSlicesFn =
    Box<dyn for<'a> Fn(&'a [SliceDesc]) -> BoxFuture<'a, Result<(), MetaError>> + Send + Sync>;

pub(crate) struct WithDataFn {
    compact: WriteCompactSliceFn,
    delete: DeleteSlicesFn,
}

impl WithDataFn {
    pub(crate) fn new(compact: WriteCompactSliceFn, delete: DeleteSlicesFn) -> Self {
        Self { compact, delete }
    }
}

pub struct NoopData;

#[async_trait::async_trait]
impl WithData for NoopData {
    async fn write_compact_slice(
        &self,
        _new_id: u64,
        _offset: u64,
        _length: u64,
        _old: &[SliceDesc],
    ) -> Result<SliceDesc, MetaError> {
        Err(MetaError::Internal(
            "compact slice data op is not configured".to_string(),
        ))
    }

    async fn delete_slices(&self, _slices: &[SliceDesc]) -> Result<(), MetaError> {
        Err(MetaError::Internal(
            "delete slices op is not configured".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl WithData for WithDataFn {
    async fn write_compact_slice(
        &self,
        new_id: u64,
        offset: u64,
        length: u64,
        old: &[SliceDesc],
    ) -> Result<SliceDesc, MetaError> {
        (self.compact)(new_id, offset, length, old).await
    }

    async fn delete_slices(&self, slices: &[SliceDesc]) -> Result<(), MetaError> {
        (self.delete)(slices).await
    }
}

pub(crate) fn default_data_op<B>(layout: ChunkLayout, store: Arc<B>) -> Arc<WithDataFn>
where
    B: BlockStore + Send + Sync + 'static,
{
    Arc::new(WithDataFn::new(
        default_write_compact_slice(layout, store.clone()),
        default_delete_slices(layout, store),
    ))
}

pub(crate) fn default_write_compact_slice<B>(
    layout: ChunkLayout,
    store: Arc<B>,
) -> WriteCompactSliceFn
where
    B: BlockStore + Send + Sync + 'static,
{
    Box::new(move |new_id, offset, length, old| {
        let store = store.clone();

        Box::pin(async move {
            let first = old
                .first()
                .ok_or_else(|| MetaError::Internal("empty compact slices".to_string()))?;

            let mut futures = FuturesUnordered::new();

            for slice in old {
                let store = store.clone();

                let fut = async move {
                    if slice.slice_id == 0 {
                        let bufs = make_zero_bytes(slice.length.as_usize());

                        return Ok::<Vec<Bytes>, MetaError>(bufs);
                    }

                    let mut fetcher = DataFetcher::new(layout, slice.chunk_id, store.as_ref());
                    fetcher.prepare_slices(vec![*slice]).await;

                    let buf = fetcher
                        .read_at(slice.offset, slice.length.as_usize())
                        .await
                        .map_err(|e| MetaError::Internal(e.to_string()))?;
                    Ok::<Vec<Bytes>, MetaError>(vec![Bytes::from_owner(buf)])
                };

                futures.push(fut);
            }

            let mut bufs = Vec::with_capacity(old.len());

            while let Some(res) = futures.next().await {
                bufs.extend(res?);
            }

            let total_len = bufs.iter().map(|b| b.len()).sum::<usize>();
            if length != 0 && total_len as u64 != length {
                return Err(MetaError::Internal(format!(
                    "compacted length mismatch: expect {length}, got {total_len}"
                )));
            }

            let uploader = DataUploader::new(layout, first.chunk_id, store.as_ref());
            uploader
                .write_at_vectored(new_id, offset, &bufs)
                .await
                .map_err(|e| MetaError::Internal(e.to_string()))
        })
    })
}

pub(crate) fn default_delete_slices<B>(layout: ChunkLayout, store: Arc<B>) -> DeleteSlicesFn
where
    B: BlockStore + Send + Sync + 'static,
{
    Box::new(move |slices| {
        let store = store.clone();

        Box::pin(async move {
            let mut futures = FuturesUnordered::new();
            for slice in slices {
                for span in block_span_iter(*slice, layout) {
                    let block_index = span.index.as_u32();

                    futures.push(store.delete_range((slice.slice_id, block_index), 1));
                }
            }

            while let Some(result) = futures.next().await {
                result?;
            }
            Ok(())
        })
    })
}
