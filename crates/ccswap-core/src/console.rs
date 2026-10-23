/// Where account operations print and ask.
pub trait Console {
    fn line(&mut self, text: &str);
    /// Shows `question` and returns the reply, `None` on end of input.
    fn prompt(&mut self, question: &str) -> Option<String>;
    /// Like `prompt`, without echoing the reply.
    fn secret(&mut self, question: &str) -> Option<String> {
        self.prompt(question)
    }
}
