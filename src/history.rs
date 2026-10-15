pub(crate) fn extend_match<T: Copy>(output: &mut Vec<T>, distance: usize, length: usize) {
    let original = output.len();
    if distance >= length { output.extend_from_within(original - distance..original - distance + length); }
    else if distance == 1 { output.resize(original + length, output[original - 1]); }
    else if length <= distance * 2 {
        output.extend_from_within(original - distance..original);
        output.extend_from_within(original..original + length - distance);
    } else {
        output.resize(original + length, output[original - distance]);
        output.copy_within(original - distance..original, original);
        let mut copied = distance;
        while copied < length {
            let count = copied.min(length - copied);
            output.copy_within(original..original + count, original + copied);
            copied += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_matches_repeat_the_requested_history() {
        for initial_len in 1..20 {
            for distance in 1..=initial_len {
                for length in 0..80 {
                    let mut expected: Vec<_> = (0..initial_len as u16).collect();
                    for _ in 0..length { expected.push(expected[expected.len() - distance]); }
                    let mut actual: Vec<_> = (0..initial_len as u16).collect();
                    extend_match(&mut actual, distance, length);
                    assert_eq!(actual, expected, "initial={initial_len}, distance={distance}, length={length}");
                }
            }
        }
    }
}
