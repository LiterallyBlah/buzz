//! The wake-word (KWS) model: manifest entry, download and attribution.
//!
//! Split out of `models.rs` rather than added to it — the shared download
//! machinery (`ModelSlot`, integrity hashing, atomic install) stays there and
//! is reused verbatim; everything specific to the `ambientVoice` preview
//! feature's model lives here, so the file that every other model shares does
//! not grow with a preview feature.
//!
//! Downloaded only when the feature is switched on, not at every launch: it is
//! ~18 MB of preview-only capability and most users will never enable it.

use super::*;

/// SHA-256 of the KWS archive
/// (sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2), verified
/// against a download from the URL below (17,626,723 bytes).
const KWS_ARCHIVE_SHA256: &str = "f170013b4716e41b62b9bfd809687c207cef798ef9bc6534d524e17af9b6561a";

/// Maximum expected KWS archive size (60 MB — actual is ~17 MB).
const MAX_KWS_DOWNLOAD_BYTES: u64 = 60 * 1024 * 1024;

/// Streaming Zipformer keyword spotter trained on GigaSpeech XL, packaged for
/// sherpa-onnx by the k2-fsa project. ~3.3 M parameters; open-vocabulary, so
/// user-typed wake words need no retraining. Same canonical host as the STT
/// model (the sherpa-onnx GitHub release assets).
/// License: Apache-2.0 (attribution required — see `KWS_LICENSE_TEXT`).
const KWS_DOWNLOAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/\
     sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01.tar.bz2";

/// Subdirectory name produced by `tar xjf` on the archive.
const KWS_ARCHIVE_SUBDIR: &str = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01";

/// Final directory name under `~/.buzz/models/`.
const KWS_MODEL_DIR_NAME: &str = "kws-zipformer-gigaspeech-en";

/// Model manifest version. Increment when upgrading model files.
const KWS_MODEL_VERSION: &str = "1";

// The archive carries both fp32 and int8 weights. Buzz installs and uses the
// **int8** set: the M0 spike measured byte-identical detections at 0.95% of a
// core versus 1.1% for fp32, from a 4.8 MB encoder instead of 12.2 MB.
pub(crate) const KWS_ENCODER: &str = "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
pub(crate) const KWS_DECODER: &str = "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
pub(crate) const KWS_JOINER: &str = "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
pub(crate) const KWS_TOKENS: &str = "tokens.txt";
/// SentencePiece Unigram vocabulary. Despite the file name it is not a BPE
/// merge table; the app tokenises wake words against it before arming the
/// engine (see `ambient_voice::wake_word`).
pub(crate) const KWS_BPE_MODEL: &str = "bpe.model";

/// Attribution sidecar written next to the KWS model files.
const KWS_LICENSE_FILE_NAME: &str = "MODEL_LICENSE.txt";

/// Files required for the wake-word model to count as ready, including the
/// attribution sidecar Buzz writes during install.
pub(crate) const KWS_REQUIRED_FILES: &[&str] = &[
    KWS_ENCODER,
    KWS_DECODER,
    KWS_JOINER,
    KWS_TOKENS,
    KWS_BPE_MODEL,
];

const KWS_EXPECTED_FILES: &[&str] = &[
    KWS_ENCODER,
    KWS_DECODER,
    KWS_JOINER,
    KWS_TOKENS,
    KWS_BPE_MODEL,
    KWS_LICENSE_FILE_NAME,
];

/// Apache-2.0 §4 attribution block written next to the wake-word model after
/// install. The upstream archive ships **no** LICENSE file — the licence is
/// declared only in the in-archive README front matter and on the ModelScope
/// repository — so Buzz supplies the notice itself and lets it travel with the
/// bytes. Mirrored in About/Credits.
const KWS_LICENSE_TEXT: &str = "\
sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01
Streaming Zipformer keyword-spotting model, trained on GigaSpeech XL.

Copyright (c) the k2-fsa / icefall project contributors.
Model author: pkufool (k2-fsa).

Licensed under the Apache License, Version 2.0 (the \"License\"); you may not
use these files except in compliance with the License. You may obtain a copy
of the License at:

    https://www.apache.org/licenses/LICENSE-2.0

Original model: https://www.modelscope.cn/pkufool/sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01
Redistributed by the sherpa-onnx project:
    https://github.com/k2-fsa/sherpa-onnx/releases/tag/kws-models
Buzz ships these files unmodified.

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an \"AS IS\" BASIS, WITHOUT
WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
License for the specific language governing permissions and limitations.
";

/// The manager's wake-word slot, so `ModelManager::new` needs no KWS detail.
pub(super) fn kws_slot() -> ModelSlot {
    ModelSlot::new(KWS_MODEL_DIR_NAME, KWS_EXPECTED_FILES, KWS_MODEL_VERSION)
}

impl ModelManager {
    /// Path to the wake-word model directory, or `None` if not ready.
    pub fn kws_model_dir(&self) -> Option<PathBuf> {
        self.kws.dir_if_ready(&self.models_dir)
    }
    /// `true` if all wake-word files are present and the manifest matches.
    pub fn is_kws_ready(&self) -> bool {
        self.kws.is_ready(&self.models_dir)
    }
    /// Current wake-word download status.
    pub fn kws_status(&self) -> ModelStatus {
        self.kws.status()
    }
    /// Returns `true` once when the wake-word model just became ready.
    pub fn take_kws_ready(&self) -> bool {
        self.kws.take_ready()
    }

    /// Start a background wake-word model download.
    ///
    /// Unlike STT/TTS this is **not** called at launch — only when the
    /// `ambientVoice` preview feature is switched on, or at boot when the
    /// persisted ambient settings say the session should run. No-op if already
    /// ready or downloading.
    pub fn start_kws_download(&self, http_client: reqwest::Client) {
        let manager = self.clone();
        self.kws.start_download(
            &self.models_dir,
            http_client,
            "wake-word",
            move |client| async move { manager.download_kws_model(client).await },
        );
    }

    /// Download, extract, verify and install the wake-word model archive.
    ///
    /// Same shape as `download_stt_model`: hash the archive before extraction,
    /// write the attribution sidecar into the staging directory so it lands in
    /// the final directory as part of the same atomic rename, then install.
    async fn download_kws_model(&self, http_client: reqwest::Client) -> Result<(), String> {
        tokio::fs::create_dir_all(&self.models_dir)
            .await
            .map_err(|e| format!("create models dir: {e}"))?;

        let archive_path = self
            .models_dir
            .join(format!("{KWS_MODEL_DIR_NAME}.tar.bz2"));
        let temp_dir = self.models_dir.join(format!("{KWS_MODEL_DIR_NAME}.tmp"));

        eprintln!("buzz-desktop: downloading wake-word model from {KWS_DOWNLOAD_URL}");
        let response = fetch_url(&http_client, KWS_DOWNLOAD_URL, "wake-word archive").await?;

        let slot = self.kws.clone();
        let bytes = download_file(
            response,
            &archive_path,
            MAX_KWS_DOWNLOAD_BYTES,
            "wake-word archive",
            |downloaded, content_length| {
                if let Some(pct) =
                    content_length.and_then(|total| (downloaded * 89).checked_div(total))
                {
                    slot.set_status(ModelStatus::Downloading {
                        progress_percent: pct.min(89) as u8,
                    });
                }
            },
        )
        .await?;
        eprintln!("buzz-desktop: downloaded {bytes} bytes, wrote to disk");

        let hash = sha256_file(&archive_path).await?;
        if hash != KWS_ARCHIVE_SHA256 {
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(format!(
                "wake-word archive integrity check failed: expected {KWS_ARCHIVE_SHA256}, got {hash}"
            ));
        }

        self.kws.set_status(ModelStatus::Downloading {
            progress_percent: 90,
        });
        fresh_temp_dir(&temp_dir).await?;

        eprintln!("buzz-desktop: extracting wake-word archive…");
        let (archive, target) = (archive_path.clone(), temp_dir.clone());
        tokio::task::spawn_blocking(move || extract_archive(&archive, &target))
            .await
            .map_err(|e| format!("tar task panicked: {e}"))??;

        let extracted_subdir = temp_dir.join(KWS_ARCHIVE_SUBDIR);
        if !extracted_subdir.is_dir() {
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(format!(
                "expected subdir '{KWS_ARCHIVE_SUBDIR}' not found after extraction"
            ));
        }

        // Apache-2.0 §4(a)/(d): the upstream tarball carries no LICENSE or
        // NOTICE, so Buzz writes the attribution itself. It ships inside the
        // model directory, so a user who copies the directory keeps it.
        let license_path = extracted_subdir.join(KWS_LICENSE_FILE_NAME);
        if let Err(e) = tokio::fs::write(&license_path, KWS_LICENSE_TEXT).await {
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(format!("write wake-word license sidecar: {e}"));
        }

        if let Err(e) = self
            .kws
            .verify_and_install(&self.models_dir, &extracted_subdir, Some(&temp_dir))
            .await
        {
            let _ = tokio::fs::remove_dir_all(&temp_dir).await;
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(e);
        }
        let _ = tokio::fs::remove_file(&archive_path).await;

        eprintln!(
            "buzz-desktop: wake-word model ready at {}",
            self.kws.model_dir(&self.models_dir).display()
        );
        Ok(())
    }
}

/// Path to the wake-word model directory, or `None` if not ready.
pub fn kws_model_dir() -> Option<PathBuf> {
    global_model_manager()?.kws_model_dir()
}

/// `true` if all expected wake-word model files are present on disk.
pub fn is_kws_ready() -> bool {
    global_model_manager()
        .map(|m| m.is_kws_ready())
        .unwrap_or(false)
}
