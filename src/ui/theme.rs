use console::style;

pub fn paste(text: &str) -> String {
    style(text).color256(222).on_color256(236).to_string()
}

pub fn accent(text: &str) -> String {
    style(text).color256(117).bold().to_string()
}
pub fn muted(text: &str) -> String {
    style(text).dim().to_string()
}
pub fn border(text: &str) -> String {
    style(text).color256(240).to_string()
}
pub fn selected(text: &str) -> String {
    style(text)
        .color256(159)
        .on_color256(237)
        .bold()
        .to_string()
}
pub fn title(text: &str) -> String {
    style(text).color256(153).bold().to_string()
}
pub fn success(text: &str) -> String {
    style(text).color256(114).to_string()
}
pub fn warning(text: &str) -> String {
    style(text).color256(215).to_string()
}
