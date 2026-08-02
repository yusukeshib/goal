use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Result, anyhow, bail};

use crate::state::HumanQuestion;

pub fn read_answer(question: &HumanQuestion, cancelled: Arc<AtomicBool>) -> Result<String> {
    println!("\n{}", question.question);
    if let Some(context) = &question.context {
        println!("\nContext:\n{context}");
    }
    print!("\n> ");
    use io::Write;
    io::stdout().flush()?;

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut answer = String::new();
        let result = io::stdin()
            .read_line(&mut answer)
            .map(|read| (read, answer));
        let _ = sender.send(result);
    });
    loop {
        if cancelled.load(Ordering::SeqCst) {
            bail!("interrupted while awaiting human input; question remains pending");
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok((0, _))) => bail!("stdin reached EOF; question remains pending"),
            Ok(Ok((_, answer))) if answer.trim().is_empty() => {
                bail!("empty human answer; question remains pending")
            }
            Ok(Ok((_, answer))) => return Ok(answer.trim_end().to_owned()),
            Ok(Err(error)) => return Err(anyhow!(error).context("read human answer")),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("human input reader stopped"),
        }
    }
}
