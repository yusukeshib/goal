use std::{
    io::{self, IsTerminal},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use reedline::{
    DefaultPrompt, DefaultPromptSegment, Emacs, Reedline, Signal, default_emacs_keybindings,
};

use crate::{cancel::Interrupted, output::Output, state::HumanQuestion};

pub struct HumanInput {
    cancelled: Arc<AtomicBool>,
    editor: Reedline,
}

impl HumanInput {
    pub fn new(cancelled: Arc<AtomicBool>) -> Self {
        let editor = Reedline::create()
            .with_edit_mode(Box::new(Emacs::new(default_emacs_keybindings())))
            .with_break_signal(Arc::clone(&cancelled));
        Self { cancelled, editor }
    }

    pub fn read_answer(&mut self, question: &HumanQuestion, output: &Output) -> Result<String> {
        output.event("human_question", serde_json::to_value(question)?)?;
        let mut prompt = format!("\n{}", question.question);
        if let Some(context) = &question.context {
            prompt.push_str(&format!("\n\nContext:\n{context}"));
        }

        if output.is_plain() && io::stdin().is_terminal() && io::stdout().is_terminal() {
            prompt.push_str(
                "\n\n[Enter: send | Shift+Enter / Alt+Enter: newline | Ctrl+C: cancel | Ctrl+R: history]\n",
            );
            output.plain_stdout(&prompt)?;
            self.read_interactive(output)
        } else {
            prompt.push_str("\n\n> ");
            output.plain_stdout(&prompt)?;
            self.read_stream(output)
        }
    }

    fn read_interactive(&mut self, output: &Output) -> Result<String> {
        let prompt = DefaultPrompt::new(DefaultPromptSegment::Empty, DefaultPromptSegment::Empty);
        loop {
            match self
                .editor
                .read_line(&prompt)
                .context("read human answer from terminal")?
            {
                Signal::Success(answer) => match finish_answer(answer) {
                    Ok(answer) => return Ok(answer),
                    Err(error) => output.plain_stderr(&format!("{error}\n"))?,
                },
                Signal::CtrlC | Signal::ExternalBreak(_) => return Err(Interrupted.into()),
                Signal::CtrlD => bail!("stdin reached EOF; question remains pending"),
                Signal::HostCommand(_) => {
                    bail!("human input editor returned an unexpected command")
                }
                _ => bail!("human input editor returned an unexpected signal"),
            }
        }
    }

    fn read_stream(&self, output: &Output) -> Result<String> {
        loop {
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let mut answer = String::new();
                let result = io::stdin()
                    .read_line(&mut answer)
                    .map(|read| (read, answer));
                let _ = sender.send(result);
            });
            loop {
                if self.cancelled.load(Ordering::SeqCst) {
                    return Err(Interrupted.into());
                }
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok((0, _))) => bail!("stdin reached EOF; question remains pending"),
                    Ok(Ok((_, answer))) => match finish_answer(answer) {
                        Ok(answer) => return Ok(answer),
                        Err(error) => {
                            output.plain_stderr(&format!("{error}\n"))?;
                            output.plain_stdout("> ")?;
                            break;
                        }
                    },
                    Ok(Err(error)) => return Err(anyhow!(error).context("read human answer")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("human input reader stopped")
                    }
                }
            }
        }
    }
}

fn finish_answer(answer: String) -> Result<String> {
    if answer.trim().is_empty() {
        bail!("empty human answer; question remains pending")
    }
    Ok(answer.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::finish_answer;

    #[test]
    fn preserves_japanese_and_internal_newlines() {
        assert_eq!(
            finish_answer("日本語で回答します。\n次の行です。\n".into()).unwrap(),
            "日本語で回答します。\n次の行です。"
        );
    }

    #[test]
    fn rejects_whitespace_only_answers() {
        assert!(finish_answer(" \n\t".into()).is_err());
    }
}
