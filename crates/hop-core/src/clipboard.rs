use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clipboard_rs::{Clipboard, ClipboardContext, ContentFormat};
use hop_protocol::control::{ClipboardContent, ClipboardFile, ControlMessage};
use sha2::{Digest, Sha256};

pub(crate) const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);

const MAX_CLIPBOARD_TEXT_BYTES: usize = 1_000_000;
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CLIPBOARD_FILE_COUNT: usize = 8;
const MAX_SINGLE_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_FILE_BYTES: usize = 24 * 1024 * 1024;
const STAGING_DIR_RETENTION: usize = 10;

#[cfg(target_os = "windows")]
const PNG_WRITE_FORMAT: &str = "PNG";
#[cfg(target_os = "macos")]
const PNG_WRITE_FORMAT: &str = "public.png";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const PNG_WRITE_FORMAT: &str = "image/png";

const PNG_READ_FORMATS: [&str; 3] = ["public.png", "PNG", "image/png"];

pub(crate) struct ClipboardSync {
    machine_name: String,
    backend: Box<dyn ClipboardBackend>,
    last_local_fingerprint: Option<[u8; 32]>,
    last_remote_sequence: u64,
    next_sequence: u64,
}

impl ClipboardSync {
    pub(crate) fn new(machine_name: &str) -> Self {
        let backend: Box<dyn ClipboardBackend> =
            match SystemClipboardBackend::new(machine_name.to_owned()) {
                Ok(backend) => Box::new(backend),
                Err(error) => {
                    eprintln!(
                        "clipboard sync disabled: failed to initialize clipboard context: {error}"
                    );
                    Box::new(NoopClipboardBackend)
                }
            };

        Self {
            machine_name: machine_name.to_owned(),
            backend,
            last_local_fingerprint: None,
            last_remote_sequence: 0,
            next_sequence: 1,
        }
    }

    pub(crate) fn poll_local_update(&mut self, enabled: bool) -> Option<ControlMessage> {
        if !enabled {
            return None;
        }

        let content = match self.backend.read_clipboard_content() {
            Ok(Some(content)) => content,
            Ok(None) => return None,
            Err(error) => {
                eprintln!("clipboard read failed: {error}");
                return None;
            }
        };

        let fingerprint = clipboard_fingerprint(&content);
        if self.last_local_fingerprint == Some(fingerprint) {
            return None;
        }

        self.last_local_fingerprint = Some(fingerprint);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);

        Some(ControlMessage::ClipboardSync {
            source_machine: self.machine_name.clone(),
            sequence,
            sent_at_micros: now_micros(),
            content,
        })
    }

    pub(crate) fn on_control_channel_reset(&mut self) {
        self.last_remote_sequence = 0;
    }

    pub(crate) fn apply_remote_update(
        &mut self,
        source_machine: &str,
        sequence: u64,
        content: ClipboardContent,
    ) {
        if source_machine == self.machine_name {
            return;
        }
        if sequence <= self.last_remote_sequence {
            return;
        }

        if let Err(error) = self
            .backend
            .write_clipboard_content(source_machine, sequence, &content)
        {
            eprintln!("failed to apply remote clipboard update: {error}");
            return;
        }

        self.last_remote_sequence = sequence;
        self.last_local_fingerprint = Some(clipboard_fingerprint(&content));
    }
}

trait ClipboardBackend {
    fn read_clipboard_content(&mut self) -> Result<Option<ClipboardContent>>;
    fn write_clipboard_content(
        &mut self,
        source_machine: &str,
        sequence: u64,
        content: &ClipboardContent,
    ) -> Result<()>;
}

struct NoopClipboardBackend;

impl ClipboardBackend for NoopClipboardBackend {
    fn read_clipboard_content(&mut self) -> Result<Option<ClipboardContent>> {
        Ok(None)
    }

    fn write_clipboard_content(
        &mut self,
        _source_machine: &str,
        _sequence: u64,
        _content: &ClipboardContent,
    ) -> Result<()> {
        Ok(())
    }
}

struct SystemClipboardBackend {
    context: ClipboardContext,
    staging_root: PathBuf,
}

impl SystemClipboardBackend {
    fn new(machine_name: String) -> Result<Self> {
        let context = ClipboardContext::new()
            .map_err(|error| anyhow::anyhow!("ClipboardContext::new failed: {error}"))?;
        let staging_root = std::env::temp_dir()
            .join("hop-clipboard")
            .join(sanitize_path_segment(&machine_name));
        Ok(Self {
            context,
            staging_root,
        })
    }

    fn read_files(&self) -> Result<Option<Vec<ClipboardFile>>> {
        if !self.context.has(ContentFormat::Files) {
            return Ok(None);
        }

        let entries = self.context.get_files().unwrap_or_default();
        if entries.is_empty() {
            return Ok(None);
        }

        let mut files = Vec::new();
        let mut used_names = HashSet::new();
        let mut total_bytes = 0_usize;

        for entry in entries {
            if files.len() >= MAX_CLIPBOARD_FILE_COUNT {
                break;
            }

            let Some(path) = clipboard_entry_to_path(&entry) else {
                continue;
            };

            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if !metadata.is_file() {
                continue;
            }

            let file_size = metadata.len() as usize;
            if file_size > MAX_SINGLE_FILE_BYTES {
                eprintln!(
                    "skipping clipboard file {} ({} bytes > {} bytes)",
                    path.display(),
                    file_size,
                    MAX_SINGLE_FILE_BYTES
                );
                continue;
            }
            if total_bytes.saturating_add(file_size) > MAX_TOTAL_FILE_BYTES {
                break;
            }

            let bytes = fs::read(&path)
                .with_context(|| format!("failed to read clipboard file {}", path.display()))?;
            let base_name = path
                .file_name()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| "clipboard-file".to_owned());
            let name = unique_file_name(&base_name, &mut used_names);
            total_bytes = total_bytes.saturating_add(bytes.len());
            files.push(ClipboardFile { name, bytes });
        }

        if files.is_empty() {
            Ok(None)
        } else {
            Ok(Some(files))
        }
    }

    fn read_png_image(&self) -> Result<Option<Vec<u8>>> {
        let formats = self.context.available_formats().unwrap_or_default();
        let matching_format = formats
            .iter()
            .find(|format| {
                PNG_READ_FORMATS
                    .iter()
                    .any(|candidate| format.eq_ignore_ascii_case(candidate))
            })
            .map(String::as_str);

        let Some(format) = matching_format else {
            return Ok(None);
        };

        let png = self.context.get_buffer(format).map_err(|error| {
            anyhow::anyhow!("failed to read clipboard image format {format}: {error}")
        })?;
        if png.is_empty() {
            return Ok(None);
        }
        if png.len() > MAX_CLIPBOARD_IMAGE_BYTES {
            eprintln!(
                "skipping clipboard image ({} bytes > {} bytes)",
                png.len(),
                MAX_CLIPBOARD_IMAGE_BYTES
            );
            return Ok(None);
        }
        Ok(Some(png))
    }

    fn read_text(&self) -> Option<String> {
        let text = self.context.get_text().ok()?;
        if text.is_empty() {
            return None;
        }
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            eprintln!(
                "skipping clipboard text ({} bytes > {} bytes)",
                text.len(),
                MAX_CLIPBOARD_TEXT_BYTES
            );
            return None;
        }
        Some(text)
    }

    fn write_staged_files(
        &mut self,
        source_machine: &str,
        sequence: u64,
        files: &[ClipboardFile],
    ) -> Result<Vec<String>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }

        fs::create_dir_all(&self.staging_root).with_context(|| {
            format!(
                "failed to create clipboard staging root {}",
                self.staging_root.display()
            )
        })?;

        let stage_dir = self.staging_root.join(format!(
            "from-{}-{}",
            sanitize_path_segment(source_machine),
            sequence
        ));
        if stage_dir.exists() {
            fs::remove_dir_all(&stage_dir).with_context(|| {
                format!(
                    "failed to clear clipboard staging dir {}",
                    stage_dir.display()
                )
            })?;
        }
        fs::create_dir_all(&stage_dir).with_context(|| {
            format!(
                "failed to create clipboard staging dir {}",
                stage_dir.display()
            )
        })?;

        let mut total_bytes = 0_usize;
        let mut used_names = HashSet::new();
        let mut staged_paths = Vec::new();

        for (index, file) in files.iter().enumerate() {
            if index >= MAX_CLIPBOARD_FILE_COUNT {
                break;
            }
            if file.bytes.len() > MAX_SINGLE_FILE_BYTES {
                continue;
            }
            if total_bytes.saturating_add(file.bytes.len()) > MAX_TOTAL_FILE_BYTES {
                break;
            }
            total_bytes = total_bytes.saturating_add(file.bytes.len());

            let requested_name = if file.name.trim().is_empty() {
                format!("clipboard-file-{}", index + 1)
            } else {
                file.name.clone()
            };
            let safe_name = unique_file_name(&requested_name, &mut used_names);
            let target = stage_dir.join(&safe_name);
            fs::write(&target, &file.bytes)
                .with_context(|| format!("failed to write staged file {}", target.display()))?;
            staged_paths.push(target.to_string_lossy().to_string());
        }

        self.cleanup_staging_dirs()?;
        Ok(staged_paths)
    }

    fn cleanup_staging_dirs(&self) -> Result<()> {
        let mut directories = Vec::new();
        for entry in fs::read_dir(&self.staging_root).with_context(|| {
            format!(
                "failed to list clipboard staging root {}",
                self.staging_root.display()
            )
        })? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
                directories.push((entry.path(), modified));
            }
        }
        directories.sort_by_key(|(_, modified)| *modified);
        while directories.len() > STAGING_DIR_RETENTION {
            if let Some((path, _)) = directories.first().cloned() {
                let _ = fs::remove_dir_all(path);
            }
            directories.remove(0);
        }
        Ok(())
    }
}

impl ClipboardBackend for SystemClipboardBackend {
    fn read_clipboard_content(&mut self) -> Result<Option<ClipboardContent>> {
        if let Some(files) = self.read_files()? {
            return Ok(Some(ClipboardContent::Files { files }));
        }
        if let Some(png_bytes) = self.read_png_image()? {
            return Ok(Some(ClipboardContent::ImagePng { png_bytes }));
        }
        if let Some(text) = self.read_text() {
            return Ok(Some(ClipboardContent::Text { text }));
        }
        Ok(None)
    }

    fn write_clipboard_content(
        &mut self,
        source_machine: &str,
        sequence: u64,
        content: &ClipboardContent,
    ) -> Result<()> {
        match content {
            ClipboardContent::Text { text } => {
                if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                    anyhow::bail!(
                        "clipboard text exceeds limit: {} > {} bytes",
                        text.len(),
                        MAX_CLIPBOARD_TEXT_BYTES
                    );
                }
                self.context
                    .set_text(text.clone())
                    .map_err(|error| anyhow::anyhow!("failed to set clipboard text: {error}"))?;
            }
            ClipboardContent::ImagePng { png_bytes } => {
                if png_bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
                    anyhow::bail!(
                        "clipboard image exceeds limit: {} > {} bytes",
                        png_bytes.len(),
                        MAX_CLIPBOARD_IMAGE_BYTES
                    );
                }
                self.context
                    .set_buffer(PNG_WRITE_FORMAT, png_bytes.clone())
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "failed to set clipboard image format {PNG_WRITE_FORMAT}: {error}"
                        )
                    })?;
            }
            ClipboardContent::Files { files } => {
                let staged_paths = self.write_staged_files(source_machine, sequence, files)?;
                if staged_paths.is_empty() {
                    anyhow::bail!("remote clipboard files were empty after applying limits");
                }
                self.context.set_files(staged_paths).map_err(|error| {
                    anyhow::anyhow!("failed to set clipboard file list: {error}")
                })?;
            }
        }
        Ok(())
    }
}

fn clipboard_entry_to_path(entry: &str) -> Option<PathBuf> {
    let raw = entry.trim();
    if raw.is_empty() {
        return None;
    }

    if let Some(url_path) = raw.strip_prefix("file://") {
        let decoded = percent_decode(url_path);
        #[cfg(target_os = "windows")]
        let decoded = normalize_windows_file_url_path(decoded);
        return Some(PathBuf::from(decoded));
    }

    Some(PathBuf::from(raw))
}

#[cfg(target_os = "windows")]
fn normalize_windows_file_url_path(path: String) -> String {
    let mut normalized = path;
    if normalized.starts_with('/') && normalized.chars().nth(2) == Some(':') {
        normalized.remove(0);
    }
    normalized
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' && idx + 2 < bytes.len() {
            let first = bytes[idx + 1] as char;
            let second = bytes[idx + 2] as char;
            if let (Some(high), Some(low)) = (first.to_digit(16), second.to_digit(16)) {
                decoded.push((high * 16 + low) as u8);
                idx += 3;
                continue;
            }
        }
        decoded.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn sanitize_path_segment(value: &str) -> String {
    let mut cleaned = value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '\0' => '_',
            _ => ch,
        })
        .collect::<String>();
    if cleaned.trim().is_empty() {
        cleaned = "unknown".to_owned();
    }
    cleaned
}

fn unique_file_name(candidate: &str, used_names: &mut HashSet<String>) -> String {
    let base = sanitize_path_segment(candidate);
    if used_names.insert(base.clone()) {
        return base;
    }
    let mut suffix = 2_u32;
    loop {
        let named = format!("{base}-{suffix}");
        if used_names.insert(named.clone()) {
            return named;
        }
        suffix = suffix.saturating_add(1);
    }
}

fn clipboard_fingerprint(content: &ClipboardContent) -> [u8; 32] {
    let mut hasher = Sha256::new();
    match content {
        ClipboardContent::Text { text } => {
            hasher.update([0x01]);
            hasher.update((text.len() as u64).to_be_bytes());
            hasher.update(text.as_bytes());
        }
        ClipboardContent::ImagePng { png_bytes } => {
            hasher.update([0x02]);
            hasher.update((png_bytes.len() as u64).to_be_bytes());
            hasher.update(png_bytes);
        }
        ClipboardContent::Files { files } => {
            hasher.update([0x03]);
            hasher.update((files.len() as u64).to_be_bytes());
            for file in files {
                hasher.update((file.name.len() as u64).to_be_bytes());
                hasher.update(file.name.as_bytes());
                hasher.update((file.bytes.len() as u64).to_be_bytes());
                hasher.update(&file.bytes);
            }
        }
    }
    hasher.finalize().into()
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct MockClipboardBackend {
        reads: VecDeque<Option<ClipboardContent>>,
        writes: Arc<Mutex<Vec<(String, u64, ClipboardContent)>>>,
    }

    impl MockClipboardBackend {
        fn with_reads(
            reads: Vec<Option<ClipboardContent>>,
            writes: Arc<Mutex<Vec<(String, u64, ClipboardContent)>>>,
        ) -> Self {
            Self {
                reads: VecDeque::from(reads),
                writes,
            }
        }
    }

    impl ClipboardBackend for MockClipboardBackend {
        fn read_clipboard_content(&mut self) -> Result<Option<ClipboardContent>> {
            Ok(self.reads.pop_front().flatten())
        }

        fn write_clipboard_content(
            &mut self,
            source_machine: &str,
            sequence: u64,
            content: &ClipboardContent,
        ) -> Result<()> {
            self.writes.lock().expect("lock writes").push((
                source_machine.to_owned(),
                sequence,
                content.clone(),
            ));
            Ok(())
        }
    }

    fn test_sync_with_backend(backend: Box<dyn ClipboardBackend>) -> ClipboardSync {
        ClipboardSync {
            machine_name: "local".to_owned(),
            backend,
            last_local_fingerprint: None,
            last_remote_sequence: 0,
            next_sequence: 1,
        }
    }

    #[test]
    fn poll_local_update_emits_only_on_change() {
        let text = ClipboardContent::Text {
            text: "hello".to_owned(),
        };
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = MockClipboardBackend::with_reads(
            vec![
                Some(text.clone()),
                Some(text.clone()),
                Some(ClipboardContent::Text {
                    text: "changed".to_owned(),
                }),
            ],
            writes,
        );
        let mut sync = test_sync_with_backend(Box::new(backend));

        let first = sync.poll_local_update(true);
        assert!(matches!(
            first,
            Some(ControlMessage::ClipboardSync { sequence: 1, .. })
        ));

        assert!(sync.poll_local_update(true).is_none());

        let third = sync.poll_local_update(true);
        assert!(matches!(
            third,
            Some(ControlMessage::ClipboardSync { sequence: 2, .. })
        ));
    }

    #[test]
    fn apply_remote_update_suppresses_rebroadcast_of_same_content() {
        let text = ClipboardContent::Text {
            text: "remote-value".to_owned(),
        };
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = MockClipboardBackend::with_reads(vec![Some(text.clone())], writes);
        let mut sync = test_sync_with_backend(Box::new(backend));

        sync.apply_remote_update("peer", 7, text);
        assert!(sync.poll_local_update(true).is_none());
    }

    #[test]
    fn stale_remote_sequence_is_ignored() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let backend = MockClipboardBackend::with_reads(vec![], writes.clone());
        let mut sync = test_sync_with_backend(Box::new(backend));

        sync.apply_remote_update(
            "peer",
            10,
            ClipboardContent::Text {
                text: "latest".to_owned(),
            },
        );
        sync.apply_remote_update(
            "peer",
            9,
            ClipboardContent::Text {
                text: "stale".to_owned(),
            },
        );

        let writes = writes.lock().expect("lock writes");
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].1, 10);
    }

    #[test]
    fn percent_decode_handles_basic_file_urls() {
        assert_eq!(
            percent_decode("/tmp/hello%20world.txt"),
            "/tmp/hello world.txt"
        );
        assert_eq!(percent_decode("/tmp/%E2%9C%93.txt"), "/tmp/✓.txt");
    }
}
