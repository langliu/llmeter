use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, img, px};
use llmeter_core::Provider;

pub(crate) fn provider_logo(provider: Provider, size: f32) -> AnyElement {
    let path = match provider {
        Provider::Codex => "providers/openai.svg",
        Provider::Claude => "providers/claude.svg",
        Provider::OpenCode => "providers/opencode.svg",
        Provider::Pi => "providers/pi.svg",
    };

    div()
        .size(px(size))
        .flex_shrink_0()
        .child(img(path).size(px(size)))
        .into_any_element()
}
