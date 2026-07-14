/// Draw a box around text lines using Unicode box-drawing characters.
///
/// The box width adapts to the longest line. Empty lines are drawn with
/// a fixed left margin for consistent visual spacing.
pub fn print_box(lines: &[&str]) {
    // Use character count, not byte length: accented characters (é, è, à...)
    // take 2 bytes in UTF-8 but only 1 visual column, so `.len()` would
    // miscalculate padding whenever lines have a different number of accents.
    let max_len = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    // Inner width (between the ║ walls): max line + 4 (2 spaces left + 2 right)
    let inner = max_len + 4;
    // Full width: ║ + inner + ║ = inner + 2

    let top = format!("╔{}╗", "═".repeat(inner));
    let empty = format!("║{}║", " ".repeat(inner));

    println!("{top}");
    for line in lines {
        if line.is_empty() {
            println!("{empty}");
        } else {
            // ║ + "  " + line + pad + " ║"  →  total length = inner + 2
            let line_len = line.chars().count();
            let pad = max_len + 1 - line_len;
            println!("║  {}{} ║", line, " ".repeat(pad));
        }
    }
    println!("╚{}╝", "═".repeat(inner));
}