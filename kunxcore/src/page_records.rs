//! Find duplicate mapping records without rearranging the live record table.
//! Preserve the earliest record, including its ownership/reference-count flags.
extern crate alloc;
use alloc::vec::Vec;

pub fn duplicate_indices(
    len: usize,
    key: impl Fn(usize) -> Option<(usize, usize)>,
) -> Result<Vec<usize>, ()> {
    let mut indices = Vec::new();
    indices.try_reserve_exact(len).map_err(|_| ())?;
    indices.extend(0..len);
    // Index breaks ties deterministically: keep the original first record.
    indices.sort_unstable_by_key(|&i| (key(i), i));
    let mut previous = None;
    indices.retain(|&i| {
        let current = key(i);
        let duplicate = current.is_some() && current == previous;
        previous = current;
        duplicate
    });
    Ok(indices)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserve_first_record_even_when_duplicates_are_unsorted() {
        let records = [(3, 9, 1), (1, 7, 2), (3, 9, 4), (1, 7, 8), (3, 9, 16)];
        let duplicate = duplicate_indices(records.len(), |i| Some((records[i].0, records[i].1))).unwrap();
        assert_eq!(duplicate, [3, 2, 4]);
        let kept: Vec<_> = records.iter().enumerate().filter(|(i, _)| !duplicate.contains(i))
            .map(|(_, r)| *r).collect();
        assert_eq!(kept, [(3, 9, 1), (1, 7, 2)]);
    }
    #[test]
    fn aliases_and_lazy_records_are_not_duplicates() {
        let records = [Some((1, 7)), Some((2, 7)), None, None, Some((1, 8))];
        assert!(duplicate_indices(records.len(), |i| records[i]).unwrap().is_empty());
    }
    #[test]
    fn agrees_with_first_match_for_many_page_patterns() {
        let mut records = Vec::new();
        let mut random = 7usize;
        for _ in 0..2000 {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            records.push(if random % 11 == 0 { None } else { Some((random % 97, random % 41)) });
        }
        let duplicates = duplicate_indices(records.len(), |i| records[i]).unwrap();
        for i in 0..records.len() {
            let expected = records[i].is_some() && records[..i].contains(&records[i]);
            assert_eq!(duplicates.contains(&i), expected, "record {i}");
        }
    }
    #[test]
    fn empty_and_large_unique_tables() {
        assert!(duplicate_indices(0, |_| None).unwrap().is_empty());
        assert!(duplicate_indices(65536, |i| Some((i * 4096, i * 4096))).unwrap().is_empty());
    }
}
