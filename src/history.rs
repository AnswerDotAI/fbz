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
