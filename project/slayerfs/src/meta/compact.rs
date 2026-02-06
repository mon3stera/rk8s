use crate::chuck::SliceDesc;
use crate::utils::Intervals;

pub(crate) fn skip_slices(slices: &[SliceDesc]) -> (Vec<SliceDesc>, u64, u64, usize) {
    let mut skipped = 0;

    while skipped < slices.len() {
        let original = slices[skipped];

        let (dry_slices, dry_offset, dry_length) = split_slices(&slices[skipped..]);

        // `compact_slices` ensure there is at least one slice in compaction, so the `unwrap` doesn't cause panic.
        let first = dry_slices.first().unwrap();

        // The processed first may not be original.
        if !(original.length >= (1 << 20) && original.length * 5 >= dry_length)
            || *first != original
        {
            return (dry_slices, dry_offset, dry_length, skipped);
        }

        skipped += 1;
    }
    (Vec::new(), 0, 0, slices.len() - 1)
}

pub(crate) fn split_slices(slices: &[SliceDesc]) -> (Vec<SliceDesc>, u64, u64) {
    assert!(!slices.is_empty(), "Slices cannot be empty");

    let (mut l, mut r) = (u64::MAX, u64::MIN);

    let mut chunk_id = 0;
    for slice in slices.iter() {
        let (slice_l, slice_r) = slice.range();

        chunk_id = slice.chunk_id;
        l = l.min(slice_l);
        r = r.max(slice_r);
    }

    let mut news = Vec::new();
    let mut cutter = Intervals::new(l, r);
    let mut total_length = 0;

    for slice in slices.iter().rev().copied() {
        let (slice_l, slice_r) = slice.range();

        for (new_l, new_r) in cutter.cut(slice_l, slice_r) {
            total_length += new_r - new_l;
            news.push(SliceDesc {
                slice_id: slice.slice_id,
                chunk_id,
                offset: new_l,
                length: new_r - new_l,
            });
        }
    }

    let rest = cutter.collect().into_iter().map(|(l, r)| {
        total_length += r - l;

        SliceDesc {
            slice_id: 0,
            chunk_id,
            offset: l,
            length: r - l,
        }
    });

    news.extend(rest);
    news.sort_by_key(|s| (s.offset, s.length));
    (news, l, total_length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(chunk_id: u64, offset: u64, length: u64) -> SliceDesc {
        SliceDesc {
            slice_id: 0,
            chunk_id,
            offset,
            length,
        }
    }

    fn normalize(mut slices: Vec<SliceDesc>) -> Vec<(u64, u64)> {
        slices.sort_by_key(|s| (s.offset, s.length));
        slices.into_iter().map(|s| (s.offset, s.length)).collect()
    }

    fn assert_full_cover(input: &[SliceDesc], output: &[SliceDesc]) {
        let (mut l, mut r) = (u64::MAX, u64::MIN);
        for s in input {
            let (sl, sr) = s.range();
            l = l.min(sl);
            r = r.max(sr);
        }

        let mut out = output.to_vec();
        out.sort_by_key(|s| (s.offset, s.length));
        let mut cur = l;
        for s in &out {
            assert!(s.length > 0, "zero-length slice at {}", s.offset);
            assert_eq!(s.offset, cur, "gap or overlap at {}", cur);
            cur = s.offset + s.length;
        }
        assert_eq!(cur, r, "output does not cover full range");
    }

    #[test]
    fn test_compact_slices_single() {
        let slices = vec![slice(1, 3, 7)];
        let out = split_slices(&slices);
        assert_eq!(normalize(out), normalize(slices.clone()));
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_duplicate() {
        let slices = vec![slice(1, 0, 4), slice(1, 0, 4)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![slice(1, 0, 4)]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_overlap() {
        let slices = vec![slice(1, 0, 10), slice(1, 5, 10)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![slice(1, 0, 10), slice(1, 10, 5)]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_disjoint_includes_gap() {
        let slices = vec![slice(1, 0, 4), slice(1, 10, 2)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![slice(1, 0, 4), slice(1, 4, 6), slice(1, 10, 2)]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_unsorted_multiple_gaps() {
        let slices = vec![slice(1, 10, 2), slice(1, 0, 4), slice(1, 6, 2)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![
            slice(1, 0, 4),
            slice(1, 4, 2),
            slice(1, 6, 2),
            slice(1, 8, 2),
            slice(1, 10, 2),
        ]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_nested() {
        let slices = vec![slice(1, 0, 20), slice(1, 5, 5)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![slice(1, 0, 20)]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_contiguous() {
        let slices = vec![slice(1, 0, 4), slice(1, 4, 4)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![slice(1, 0, 4), slice(1, 4, 4)]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    fn test_compact_slices_overlap_small_first() {
        let slices = vec![slice(1, 5, 2), slice(1, 0, 10), slice(1, 2, 4)];
        let out = split_slices(&slices);
        let got = normalize(out);
        let expect = normalize(vec![slice(1, 0, 5), slice(1, 5, 2), slice(1, 7, 3)]);
        assert_eq!(got, expect);
        assert_full_cover(&slices, &split_slices(&slices));
    }

    #[test]
    #[should_panic]
    fn test_compact_slices_empty_panics() {
        let slices: Vec<SliceDesc> = Vec::new();
        let _ = split_slices(&slices);
    }
}
