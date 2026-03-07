// Quick test to verify layout calculation
const RECORD_HEADER_SIZE: usize = 8;
const RECORD_ALIGNMENT: usize = 8;

const fn pad_alignment(size: usize, alignment: usize) -> usize {
    let mask = alignment - 1;
    (size + mask) & !mask
}

fn main() {
    let key_offset = pad_alignment(RECORD_HEADER_SIZE, RECORD_ALIGNMENT);
    println!("RECORD_HEADER_SIZE: {}", RECORD_HEADER_SIZE);
    println!("RECORD_ALIGNMENT: {}", RECORD_ALIGNMENT);
    println!("KEY_OFFSET (pad_alignment(8, 8)): {}", key_offset);
    assert_eq!(key_offset, 8);
    println!("KEY_OFFSET=8 is correct!");
}
