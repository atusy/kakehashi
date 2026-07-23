use crate::semantic_baseline::TRACKED_MARKER;

pub const DIRTY_STATE_COUNT: usize = 128;

pub struct DirtyRustDocument {
    pub content: String,
    pub edit_start_line: u32,
    pub edit_end_line: u32,
    pub dirty_units: usize,
    pub total_units: usize,
}

fn gen_rust_unit(unit: usize, state: usize) -> String {
    assert!(state < 1_000, "state must fit the fixed three-digit field");
    format!(
        "/// Dirty unit {unit:04}, state {state:03}.\n\
         pub fn dirty_unit_{unit:04}_state_{state:03}(input: usize) -> usize {{\n\
        \x20   let value_state_{state:03} = input + {state:03};\n\
        \x20   let label_state_{state:03} = \"state_{state:03}\";\n\
        \x20   if value_state_{state:03} % 2 == 0 {{\n\
        \x20       value_state_{state:03} + label_state_{state:03}.len()\n\
        \x20   }} else {{\n\
        \x20       value_state_{state:03}\n\
        \x20   }}\n\
         }}\n\n"
    )
}

/// Generate the contiguous replacement for `dirty_units` syntax units.
///
/// The marker's leading column equals `state`, allowing the client-side token
/// baseline to reject a stale response even though every replacement preserves
/// the number of lines and the Rust grammar remains valid.
pub fn gen_dirty_rust_block(dirty_units: usize, state: usize) -> String {
    assert!(dirty_units > 0);
    assert!(state < DIRTY_STATE_COUNT);
    let mut block = format!("{}{TRACKED_MARKER}\n", " ".repeat(state));
    for unit in 0..dirty_units {
        block.push_str(&gen_rust_unit(unit, state));
    }
    block
}

/// Generate a dense Rust document whose first `dirty_units` out of
/// `total_units` form one replaceable syntax footprint.
pub fn gen_dirty_rust(total_units: usize, dirty_units: usize) -> DirtyRustDocument {
    assert!(total_units > 0);
    assert!((1..=total_units).contains(&dirty_units));

    let prefix = "use std::collections::HashMap;\n\n";
    let edit_start_line = u32::try_from(prefix.lines().count()).expect("line count fits u32");
    let initial_block = gen_dirty_rust_block(dirty_units, 0);
    let block_lines = u32::try_from(initial_block.lines().count()).expect("line count fits u32");
    let mut content = String::with_capacity(total_units * 500);
    content.push_str(prefix);
    content.push_str(&initial_block);
    for unit in dirty_units..total_units {
        content.push_str(&gen_rust_unit(unit, 0));
    }

    DirtyRustDocument {
        content,
        edit_start_line,
        edit_end_line: edit_start_line + block_lines,
        dirty_units,
        total_units,
    }
}
