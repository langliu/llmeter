use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

const PROVIDER_ASSETS: [&str; 8] = [
    "providers/codex.svg",
    "providers/claude.svg",
    "providers/opencode.svg",
    "providers/pi.svg",
    "providers/omp.svg",
    "providers/zed.svg",
    "providers/grok.svg",
    "providers/hermes.svg",
];
const ICON_ASSETS: [&str; 1] = ["icons/layers.svg"];

pub(crate) struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let asset = match path {
            "providers/codex.svg" => {
                Some(include_bytes!("../assets/providers/codex.svg").as_slice())
            }
            "providers/claude.svg" => {
                Some(include_bytes!("../assets/providers/claude.svg").as_slice())
            }
            "providers/opencode.svg" => {
                Some(include_bytes!("../assets/providers/opencode.svg").as_slice())
            }
            "providers/pi.svg" => Some(include_bytes!("../assets/providers/pi.svg").as_slice()),
            "providers/omp.svg" => Some(include_bytes!("../assets/providers/omp.svg").as_slice()),
            "providers/zed.svg" => Some(include_bytes!("../assets/providers/zed.svg").as_slice()),
            "providers/grok.svg" => Some(include_bytes!("../assets/providers/grok.svg").as_slice()),
            "providers/hermes.svg" => {
                Some(include_bytes!("../assets/providers/hermes.svg").as_slice())
            }
            "icons/layers.svg" => Some(include_bytes!("../assets/icons/layers.svg").as_slice()),
            _ => None,
        };
        match asset {
            Some(asset) => Ok(Some(Cow::Borrowed(asset))),
            None => AssetSource::load(&gpui_component_assets::Assets, path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut assets = AssetSource::list(&gpui_component_assets::Assets, path)?;
        assets.extend(
            PROVIDER_ASSETS
                .iter()
                .filter(|asset| asset.starts_with(path))
                .map(|asset| SharedString::from(*asset)),
        );
        assets.extend(
            ICON_ASSETS
                .iter()
                .filter(|asset| asset.starts_with(path))
                .map(|asset| SharedString::from(*asset)),
        );
        Ok(assets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_all_provider_logos() {
        for path in PROVIDER_ASSETS {
            let asset = AssetSource::load(&Assets, path).unwrap();
            assert!(asset.is_some(), "missing embedded provider asset: {path}");
        }
    }

    #[test]
    fn embeds_app_icons() {
        for path in ICON_ASSETS {
            let asset = AssetSource::load(&Assets, path).unwrap();
            assert!(asset.is_some(), "missing embedded app icon: {path}");
        }
    }
}
