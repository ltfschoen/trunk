mod copy_dir;
mod copy_file;
#[cfg(test)]
mod copy_file_test;
mod css;
mod html;
mod icon;
mod inline;
mod js;
mod rust;
mod sass;
mod tailwind_css;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
pub use html::HtmlPipeline;
use nipper::Document;
use serde::Deserialize;
use tokio::fs;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::common::path_exists;
use crate::config::RtcBuild;
use crate::pipelines::copy_dir::{CopyDir, CopyDirOutput};
use crate::pipelines::copy_file::{CopyFile, CopyFileOutput};
use crate::pipelines::css::{Css, CssOutput};
use crate::pipelines::icon::{Icon, IconOutput};
use crate::pipelines::inline::{Inline, InlineOutput};
use crate::pipelines::js::{Js, JsOutput};
use crate::pipelines::rust::{RustApp, RustAppOutput};
use crate::pipelines::sass::{Sass, SassOutput};
use crate::pipelines::tailwind_css::{TailwindCss, TailwindCssOutput};

const ATTR_INLINE: &str = "data-inline";
const ATTR_HREF: &str = "href";
const ATTR_SRC: &str = "src";
const ATTR_TYPE: &str = "type";
const ATTR_REL: &str = "rel";
const SNIPPETS_DIR: &str = "snippets";
const TRUNK_ID: &str = "data-trunk-id";

/// A mapping of all attrs associated with a specific `<link data-trunk .../>` element.
pub type Attrs = HashMap<String, String>;

/// A reference to a trunk asset.
pub enum TrunkAssetReference {
    Link(Attrs),
    Script(Attrs),
}

/// A model of all of the supported Trunk asset links expressed in the source HTML as
/// `<trunk-link/>` elements.
///
/// Trunk will remove all `<trunk-link .../>` elements found in the HTML. It is the responsibility
/// of each pipeline to implement a pipeline finalizer method for its pipeline output in order to
/// update the finalized HTML for asset links and the like.
#[allow(clippy::large_enum_variant)]
pub enum TrunkAsset {
    Css(Css),
    Sass(Sass),
    TailwindCss(TailwindCss),
    Js(Js),
    Icon(Icon),
    Inline(Inline),
    CopyFile(CopyFile),
    CopyDir(CopyDir),
    RustApp(RustApp),
}

impl TrunkAsset {
    /// Construct a new instance.
    pub async fn from_html(
        cfg: Arc<RtcBuild>,
        html_dir: Arc<PathBuf>,
        ignore_chan: Option<mpsc::Sender<PathBuf>>,
        reference: TrunkAssetReference,
        id: usize,
    ) -> Result<Self> {
        match reference {
            TrunkAssetReference::Link(attrs) => {
                let rel = attrs.get(ATTR_REL).context(
                    "all <link data-trunk .../> elements must have a `rel` attribute indicating \
                     the asset type",
                )?;
                Ok(match rel.as_str() {
                    Sass::TYPE_SASS | Sass::TYPE_SCSS => {
                        Self::Sass(Sass::new(cfg, html_dir, attrs, id).await?)
                    }
                    Icon::TYPE_ICON => Self::Icon(Icon::new(cfg, html_dir, attrs, id).await?),
                    Inline::TYPE_INLINE => Self::Inline(Inline::new(html_dir, attrs, id).await?),
                    Css::TYPE_CSS => Self::Css(Css::new(cfg, html_dir, attrs, id).await?),
                    CopyFile::TYPE_COPY_FILE => {
                        Self::CopyFile(CopyFile::new(cfg, html_dir, attrs, id).await?)
                    }
                    CopyDir::TYPE_COPY_DIR => {
                        Self::CopyDir(CopyDir::new(cfg, html_dir, attrs, id).await?)
                    }
                    RustApp::TYPE_RUST_APP => {
                        Self::RustApp(RustApp::new(cfg, html_dir, ignore_chan, attrs, id).await?)
                    }
                    TailwindCss::TYPE_TAILWIND_CSS => {
                        Self::TailwindCss(TailwindCss::new(cfg, html_dir, attrs, id).await?)
                    }
                    _ => bail!(
                        r#"unknown <link data-trunk .../> attr value `rel="{}"`; please ensure the value is lowercase and is a supported asset type"#,
                        rel
                    ),
                })
            }
            TrunkAssetReference::Script(attrs) => {
                Ok(Self::Js(Js::new(cfg, html_dir, attrs, id).await?))
            }
        }
    }

    /// Spawn the build pipeline for this asset.
    pub fn spawn(self) -> JoinHandle<Result<TrunkAssetPipelineOutput>> {
        match self {
            Self::Css(inner) => inner.spawn(),
            Self::Sass(inner) => inner.spawn(),
            Self::TailwindCss(inner) => inner.spawn(),
            Self::Js(inner) => inner.spawn(),
            Self::Icon(inner) => inner.spawn(),
            Self::Inline(inner) => inner.spawn(),
            Self::CopyFile(inner) => inner.spawn(),
            Self::CopyDir(inner) => inner.spawn(),
            Self::RustApp(inner) => inner.spawn(),
        }
    }
}

/// The output of a `<trunk-link/>` asset pipeline.
pub enum TrunkAssetPipelineOutput {
    Css(CssOutput),
    Sass(SassOutput),
    TailwindCss(TailwindCssOutput),
    Js(JsOutput),
    Icon(IconOutput),
    Inline(InlineOutput),
    CopyFile(CopyFileOutput),
    CopyDir(CopyDirOutput),
    RustApp(RustAppOutput),
}

impl TrunkAssetPipelineOutput {
    pub async fn finalize(self, dom: &mut Document) -> Result<()> {
        match self {
            TrunkAssetPipelineOutput::Css(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::Sass(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::TailwindCss(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::Js(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::Icon(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::Inline(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::CopyFile(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::CopyDir(out) => out.finalize(dom).await,
            TrunkAssetPipelineOutput::RustApp(out) => out.finalize(dom).await,
        }
    }
}

/// An asset file to be processed by some build pipeline.
pub struct AssetFile {
    /// The canonicalized path to the target file.
    pub path: PathBuf,
    /// The name of the file itself.
    pub file_name: OsString,
    /// The file stem of the asset file.
    pub file_stem: OsString,
    /// The extension of the file.
    pub ext: Option<String>,
}

impl AssetFile {
    /// Create a new instance.
    ///
    /// The given path will be validated to ensure the following:
    /// - that the full canonicalized path points to a file on the FS.
    /// - that the file has a filename.
    /// - that the file has an extension.
    ///
    /// Any errors returned from this constructor indicate that one of these invariants was not
    /// upheld.
    pub async fn new(rel_dir: &Path, mut path: PathBuf) -> Result<Self> {
        // If the given path is not absolute, then we join it with the directory from which the
        // relative path should be based.
        if !path.is_absolute() {
            path = rel_dir.join(path);
        }

        // Take the path to referenced resource, if it is actually an FS path, then we continue.
        let path = fs::canonicalize(&path)
            .await
            .with_context(|| format!("error getting canonical path for {:?}", &path))?;
        ensure!(
            path_exists(&path).await?,
            "target file does not appear to exist on disk {:?}",
            &path
        );
        let file_name = match path.file_name() {
            Some(file_name) => file_name.to_owned(),
            None => bail!("asset has no file name {:?}", &path),
        };
        let file_stem = match path.file_stem() {
            Some(file_stem) => file_stem.to_owned(),
            None => bail!("asset has no file name stem {:?}", &path),
        };
        let ext = path
            .extension()
            .map(|ext| ext.to_owned().to_string_lossy().to_string());
        Ok(Self {
            path,
            file_name,
            file_stem,
            ext,
        })
    }

    /// Copy this asset to the target dir. If hashing is enabled, create a hash from the file
    /// contents and include it as hex string in the destination file name.
    ///
    /// The base file name (stripped path, without any parent folders) is returned if the operation
    /// was successful.
    pub async fn copy(&self, to_dir: &Path, with_hash: bool) -> Result<String> {
        let bytes = fs::read(&self.path)
            .await
            .with_context(|| format!("error reading file for copying {:?}", &self.path))?;

        let file_name = if with_hash {
            format!(
                "{}-{:x}.{}",
                &self.file_stem.to_string_lossy(),
                seahash::hash(bytes.as_ref()),
                &self.ext.as_deref().unwrap_or_default()
            )
        } else {
            self.file_name.to_string_lossy().into_owned()
        };

        let file_path = to_dir.join(&file_name);

        fs::write(&file_path, bytes)
            .await
            .with_context(|| format!("error copying file {:?} to {:?}", &self.path, &file_path))?;

        Ok(file_name)
    }

    /// Read the content of this asset to a String.
    pub async fn read_to_string(&self) -> Result<String> {
        fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("error reading file {:?} to string", self.path))
    }
}

/// A stage in the build process.
///
/// This is used to specify when a hook will run.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    /// The stage before asset builds are executed.
    PreBuild,
    /// The stage where all asset builds are executed.
    Build,
    /// The stage after asset builds are executed.
    PostBuild,
}

/// Create the CSS selector for selecting a trunk link by ID.
pub(self) fn trunk_id_selector(id: usize) -> String {
    format!(r#"link[{}="{}"]"#, TRUNK_ID, id)
}

/// Create the CSS selector for selecting a trunk script by ID.
pub(self) fn trunk_script_id_selector(id: usize) -> String {
    format!(r#"script[{}="{}"]"#, TRUNK_ID, id)
}
