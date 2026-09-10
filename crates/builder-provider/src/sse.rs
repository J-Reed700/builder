/// Incremental SSE decoder. Bytes are decoded only after a complete line, so
/// multi-byte UTF-8 split across network frames is preserved.
#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: Vec<String>,
    event_bytes: usize,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<String>> {
        const LIMIT: usize = 4 * 1024 * 1024;
        self.buffer.extend_from_slice(chunk);
        anyhow::ensure!(self.buffer.len() <= LIMIT, "SSE buffer exceeds 4 MiB");
        let mut events = vec![];
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let bytes: Vec<_> = self.buffer.drain(..=end).collect();
            let line = std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
                self.event_bytes = 0;
            } else if let Some(value) = line.strip_prefix("data:") {
                self.event_bytes += value.len();
                anyhow::ensure!(self.event_bytes <= LIMIT, "SSE event exceeds 4 MiB");
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).into());
            }
        }
        Ok(events)
    }
}
