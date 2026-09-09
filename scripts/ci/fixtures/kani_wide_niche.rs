//! Regression for full-width enum niche offsets in the proof compiler.

#[repr(u128)]
#[derive(Clone, Copy)]
enum Wide {
    Low = 0,
    High = 1_u128 << 100,
}

#[kani::proof]
fn wide_niche_preserves_all_variants() {
    // Causes: the enum has a valid discriminant above u64::MAX; Option uses
    // the following invalid value as its niche. R1 Low, R2 High and R3 None
    // must remain distinct. Every legal input constructor is selected, without
    // assuming the decoded discriminant or excluding the high-offset case.
    let choice: u8 = kani::any();
    kani::assume(choice < 3);
    let value = match choice {
        0 => Some(Wide::Low),
        1 => Some(Wide::High),
        _ => None,
    };
    let decoded = match value {
        Some(Wide::Low) => 0,
        Some(Wide::High) => 1,
        None => 2,
    };
    assert!(decoded == choice);
}

// Cross the multi-niche range branch with a high offset, the top bit and a
// wrapping invalid range. The last layout has valid [1, MAX-1], so its two
// available niche tags are MAX and zero: subtraction must wrap at 128 bits.
macro_rules! check_multiple_niches {
    ($proof:ident, $low:expr, $high:expr) => {
        #[kani::proof]
        fn $proof() {
            #[repr(u128)]
            enum Value {
                Low = $low,
                High = $high,
            }
            enum Packed {
                Value(Value),
                EmptyOne,
                EmptyTwo,
            }
            assert!(std::mem::size_of::<Packed>() == std::mem::size_of::<Value>());
            let choice: u8 = kani::any();
            kani::assume(choice < 4);
            let value = match choice {
                0 => Packed::Value(Value::Low),
                1 => Packed::Value(Value::High),
                2 => Packed::EmptyOne,
                _ => Packed::EmptyTwo,
            };
            let decoded = match value {
                Packed::Value(Value::Low) => 0,
                Packed::Value(Value::High) => 1,
                Packed::EmptyOne => 2,
                Packed::EmptyTwo => 3,
            };
            assert!(decoded == choice);
        }
    };
}

check_multiple_niches!(wide_multiple_niches_preserve_all_variants, 0, 1_u128 << 100);
check_multiple_niches!(top_bit_niches_preserve_all_variants, 0, 1_u128 << 127);
check_multiple_niches!(wrapping_niches_preserve_all_variants, 1, u128::MAX - 1);
