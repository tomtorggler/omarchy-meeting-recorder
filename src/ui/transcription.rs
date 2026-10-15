//! Transcription orchestration and applying a result to a meeting.
use super::*;

impl Recorder {
    /// Transcribes into `result_dir/transcript.md`, driving the animation.
    pub(super) async fn run_transcription(
        &self,
        tracks: Tracks,
        language: &'static str,
    ) -> Result<(), String> {
        self.fresh_transcript.set(false);
        let Some(out) = self.result_dir.borrow().clone() else {
            return Err("no meeting folder".into());
        };
        self.set_compact(false);
        crate::library::processing(&out).map_err(|e| format!("could not save job status: {e}"))?;
        self.shared.lock().unwrap().stage = "Waiting for transcription".into();
        self.set_state(State::Transcribing);
        if let Some(hub) = self.hub.upgrade() {
            if self.window.is_visible() {
                hub.show_home();
            } else if let Some(home) = hub.home.get() {
                home.refresh();
            }
        }
        self.animation.reset();
        self.animation.set_stage("Loading audio");
        self.animation.set_progress(0.0);
        self.animation.set_running(true);

        self.invalidate_chapters();
        let model = crate::models::configured();
        let worker_model = model.clone();
        let abort = Abort::default();
        *self.abort.borrow_mut() = Some(abort.clone());
        let (events_tx, events_rx) = async_channel::unbounded::<Event>();
        let (done_tx, done_rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result =
                crate::worker::transcribe(tracks, language, &worker_model, &events_tx, &abort);
            let _ = done_tx.send_blocking(result);
            let _ = events_tx.send_blocking(Event::Finished);
        });

        while let Ok(event) = events_rx.recv().await {
            match event {
                Event::Stage(stage) => {
                    self.animation.set_stage(&stage);
                    self.shared.lock().unwrap().stage = stage;
                }
                Event::Progress(progress) => {
                    self.animation.set_progress(progress);
                    self.shared.lock().unwrap().progress = progress;
                }
                Event::Segment(text) => {
                    // The transcription labels speakers You/Remote or Speaker N;
                    // show this meeting's names.
                    let text = match (self.manifest.borrow().as_ref(), text.split_once(": ")) {
                        (Some(m), Some((label, rest))) => {
                            let labels = m.default_labels();
                            match labels.iter().position(|l| l == label) {
                                Some(i) if i < m.speakers.len() => {
                                    format!("{}: {rest}", m.speakers[i])
                                }
                                _ => text.clone(),
                            }
                        }
                        _ => text.clone(),
                    };
                    self.animation.push_text(&text);
                }
                Event::Finished => break,
            }
        }
        let result = done_rx
            .recv()
            .await
            .unwrap_or_else(|_| Err("transcription stopped unexpectedly".into()));
        *self.abort.borrow_mut() = None;

        let transcript = result?;
        // The name as it is now; it may have been edited while transcribing.
        let out = self.result_dir.borrow().clone().unwrap_or(out);
        let date = glib::DateTime::from_unix_local(self.started_at.get())
            .and_then(|t| t.format("%Y-%m-%d %H:%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let context = self
            .manifest
            .borrow()
            .as_ref()
            .map(Manifest::context_markdown)
            .unwrap_or_default();
        let mut markdown = transcribe::to_markdown(&self.title(), &date, &transcript, &context);
        let mut updated = self.manifest.borrow().clone();
        if let Some(manifest) = updated.as_mut() {
            // An import finds its own number of speakers; keep names already
            // given and number the rest.
            if manifest.imported.is_some() {
                let found = speakers_in(&markdown);
                let mut names = manifest.speakers.clone();
                names.resize_with(found.len(), String::new);
                for (i, name) in names.iter_mut().enumerate() {
                    if name.is_empty() {
                        *name = format!("Speaker {}", i + 1);
                    }
                }
                manifest.speakers = names;
            } else {
                // A recording: You, or You 1, You 2, ... when several people
                // share the mic, then Remote or Remote 1, Remote 2, ... Keep
                // the names already given to a label; a new one gets a default.
                let mut labels: Vec<String> = speakers_in(&markdown)
                    .into_iter()
                    .filter(|l| meeting::side_of(l).is_some())
                    .collect();
                labels.sort_by_key(|l| {
                    let (side, n) = meeting::side_of(l).unwrap_or(("", 0));
                    (side != meeting::DEFAULT_YOU, n)
                });
                if !labels.is_empty() {
                    let known: std::collections::HashMap<String, String> = manifest
                        .default_labels()
                        .into_iter()
                        .zip(manifest.speakers.iter().cloned())
                        .collect();
                    let names = labels
                        .iter()
                        .map(|label| {
                            if let Some(name) = known.get(label) {
                                return name.clone();
                            }
                            // One voice where there were several: the name of the first.
                            if let Some(name) = known.get(&format!("{label} 1")) {
                                return name.clone();
                            }
                            match meeting::side_of(label) {
                                // The first voice on the mic is you.
                                Some((side, 1)) => {
                                    known.get(side).cloned().unwrap_or_else(|| label.clone())
                                }
                                Some((side, n)) if side == meeting::DEFAULT_YOU => {
                                    format!("Room {n}")
                                }
                                _ => label.clone(),
                            }
                        })
                        .collect();
                    manifest.labels = labels;
                    manifest.speakers = names;
                }
            }
            // The transcription labels speakers You/Remote or Speaker N; use
            // the names of this meeting.
            let renames: Vec<(String, String)> = manifest
                .default_labels()
                .into_iter()
                .zip(manifest.speakers.iter().cloned())
                .filter(|(label, name)| label != name)
                .collect();
            markdown = meeting::relabel_all(&markdown, &renames);
            manifest.language = language.to_owned();
            manifest.model = Some(model);
            manifest.title = self.title();
            // Chapters of a previous transcript would point at lines that are gone.
            manifest.chapters.clear();
            manifest.chapters_by = None;
        }
        match updated.as_ref() {
            Some(manifest) => meeting::update(&out, manifest, Some(&markdown)),
            None => crate::files::atomic_write(&out.join("transcript.md"), markdown.as_bytes()),
        }
        .map_err(|e| format!("could not save the transcript: {e}"))?;
        *self.manifest.borrow_mut() = updated;
        self.fresh_transcript.set(true);
        Ok(())
    }
}
