use console::style;

pub fn paste(text: &str) -> String {
    style(text).color256(147).on_color256(236).to_string()
}

pub fn accent(text: &str) -> String {
    style(text).color256(147).bold().to_string()
}
pub fn muted(text: &str) -> String {
    style(text).color256(245).to_string()
}
pub fn border(text: &str) -> String {
    style(text).color256(240).to_string()
}
pub fn selected(text: &str) -> String {
    style(text)
        .color256(189)
        .on_color256(237)
        .bold()
        .to_string()
}
pub fn title(text: &str) -> String {
    style(text).bold().to_string()
}
pub fn success(text: &str) -> String {
    style(text).color256(114).to_string()
}
pub fn warning(text: &str) -> String {
    style(text).color256(215).to_string()
}
