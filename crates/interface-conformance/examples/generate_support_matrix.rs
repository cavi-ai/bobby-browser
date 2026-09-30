use std::path::PathBuf;

use interface_conformance::{
    render_preset_matrix_markdown, render_support_matrix_markdown, PRESET_MATRIX_BEGIN,
    PRESET_MATRIX_END, SUPPORT_MATRIX_BEGIN, SUPPORT_MATRIX_END,
};

fn main() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/bobby-browser/source/pages/concepts/capabilities.md");
    let current = std::fs::read_to_string(&path).expect("read capabilities documentation");
    let generated = render_support_matrix_markdown();
    let presets = render_preset_matrix_markdown();
    let updated = replace_generated_block(&current, &generated);
    let updated = replace_marked_block(&updated, &presets, PRESET_MATRIX_BEGIN, PRESET_MATRIX_END);

    if std::env::args().any(|argument| argument == "--write") {
        std::fs::write(&path, updated).expect("write capabilities documentation");
    } else {
        println!("{generated}\n\n{presets}");
    }
}

fn replace_marked_block(current: &str, generated: &str, begin: &str, end: &str) -> String {
    let start = current
        .find(begin)
        .expect("capabilities documentation must contain the generated block's begin marker");
    let end = current[start..]
        .find(end)
        .map(|offset| start + offset + end.len())
        .expect("generated block must have an end marker");
    format!("{}{}{}", &current[..start], generated, &current[end..])
}

fn replace_generated_block(current: &str, generated: &str) -> String {
    let Some(start) = current.find(SUPPORT_MATRIX_BEGIN) else {
        let start = current
            .find("## Operation → capability matrix")
            .expect("capabilities documentation must contain the operation matrix heading");
        let end = current
            .find("## Privileged primitives")
            .expect("capabilities documentation must contain the privileged primitives heading");
        return format!("{}{}\n\n{}", &current[..start], generated, &current[end..]);
    };
    let end = current[start..]
        .find(SUPPORT_MATRIX_END)
        .map(|offset| start + offset + SUPPORT_MATRIX_END.len())
        .expect("generated support block must have an end marker");
    format!("{}{}{}", &current[..start], generated, &current[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_block_without_touching_neighbors() {
        let current = format!("before\n{SUPPORT_MATRIX_BEGIN}\nold\n{SUPPORT_MATRIX_END}\nafter\n");
        let generated = format!("{SUPPORT_MATRIX_BEGIN}\nnew\n{SUPPORT_MATRIX_END}");
        assert_eq!(
            replace_generated_block(&current, &generated),
            format!("before\n{generated}\nafter\n")
        );
    }
}
