//! Chapter jobs own their busy flag, even when their result becomes stale.
use super::*;

pub(super) struct ChapterJob {
    recorder: Rc<Recorder>,
    generation: u64,
}

impl ChapterJob {
    pub(super) fn begin(recorder: &Rc<Recorder>) -> Self {
        recorder.generating.set(true);
        Self {
            recorder: recorder.clone(),
            generation: recorder.meeting_generation.get(),
        }
    }
}

impl Drop for ChapterJob {
    fn drop(&mut self) {
        // An invalidated old task must not clear a newer task's busy flag.
        if self.recorder.meeting_generation.get() == self.generation {
            self.recorder.generating.set(false);
        }
    }
}

impl Recorder {
    /// Asks the default agent for chapters in the background. The meeting
    /// works the same without them; a failure only shows up in the header.
    pub(super) fn invalidate_chapters(&self) {
        self.meeting_generation
            .set(self.meeting_generation.get().wrapping_add(1));
        self.generating.set(false);
    }

    pub(super) fn generate_chapters(self: &Rc<Self>) {
        if self.state.get() != State::Done || self.generating.get() {
            self.run_automatic_actions();
            return;
        }
        let (Some(agent), Some(_dir)) = (self.default_agent(), self.result_dir.borrow().clone())
        else {
            self.run_automatic_actions();
            return;
        };
        let lines = self.lines.borrow().clone();
        let generation = self.meeting_generation.get();
        let job = ChapterJob::begin(self);
        self.update_chapters_header(!self.chapter_starts.borrow().is_empty());
        let this = self.clone();
        glib::spawn_future_local(async move {
            let asked = agent.clone();
            let result = gio::spawn_blocking(move || {
                let lines: Vec<chapters::Line> = lines
                    .iter()
                    .map(|(start_ms, speaker, text)| chapters::Line {
                        start_ms: *start_ms,
                        speaker,
                        text,
                    })
                    .collect();
                chapters::generate(&asked, &lines)
            })
            .await
            .unwrap_or_else(|_| Err("the agent stopped unexpectedly".into()));
            drop(job); // all exits release the flag, including stale results
            if this.meeting_generation.get() != generation || !this.state.get().has_transcript() {
                this.run_automatic_actions();
                return;
            }
            let Some(dir) = this.result_dir.borrow().clone() else {
                this.run_automatic_actions();
                return;
            };
            match result {
                Ok(list) => this.store_chapters(&dir, &list, &agent),
                Err(message) => {
                    eprintln!("{APP_NAME}: chapters: {message}");
                    this.update_chapters_header(!this.chapter_starts.borrow().is_empty());
                    this.chapters_group
                        .set_description(Some(&format!("{} could not make chapters", agent.name)));
                }
            }
            // With or without chapters, the meeting is now as complete as it
            // gets: the automatic actions can have it.
            this.run_automatic_actions();
        });
    }

    fn store_chapters(self: &Rc<Self>, dir: &std::path::Path, list: &[Chapter], agent: &Agent) {
        let Some(mut manifest) = self.manifest.borrow().clone() else {
            return;
        };
        let result = meeting::read_transcript(dir).and_then(|text| {
            let text =
                text.ok_or_else(|| std::io::Error::other("no transcript to add chapters to"))?;
            let text = chapters::apply_to_markdown(&text, list);
            manifest.chapters = list.to_vec();
            manifest.chapters_by = Some(agent.id.clone());
            meeting::update(dir, &manifest, Some(&text))?;
            Ok(text)
        });
        match result {
            Ok(text) => {
                *self.manifest.borrow_mut() = Some(manifest);
                self.show_transcript(Some(&text), None);
                self.toast(&format!("{} chapters added", list.len()));
            }
            Err(error) => self.toast(&format!("Could not save chapters: {error}")),
        }
    }
}
