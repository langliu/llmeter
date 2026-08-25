use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, img, px, rgb};
use llmeter_core::Provider;

pub(crate) fn provider_logo(provider: Provider, size: f32) -> AnyElement {
    let path = match provider {
        Provider::Codex => "providers/codex.svg",
        Provider::Claude => "providers/claude.svg",
        Provider::OpenCode => "providers/opencode.svg",
        Provider::Pi => "providers/pi.svg",
        Provider::Omp => "providers/omp.svg",
        Provider::Zed => "providers/zed.svg",
        Provider::Grok => "providers/grok.svg",
        Provider::Hermes => "providers/hermes.svg",
    };

    if matches!(
        provider,
        Provider::Claude
            | Provider::OpenCode
            | Provider::Pi
            | Provider::Zed
            | Provider::Grok
            | Provider::Hermes
    ) {
        return div()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .rounded_md()
            .bg(rgb(0xffffff))
            .child(img(path).size(px(size * 0.72)))
            .into_any_element();
    }

    div()
        .size(px(size))
        .flex_shrink_0()
        .child(img(path).size(px(size)))
        .into_any_element()
}
