use iced::{
    widget::{column, container, scrollable, text},
    Element, Length,
};

#[derive(Debug, Clone)]
pub enum ConsoleMessage {}

pub struct Console {
    logs: Vec<String>,
}

impl Console {
    pub fn new() -> Self {
        Self {
            logs: vec![
                "Console output...".to_string(),
                "Server starting...".to_string(),
            ],
        }
    }

    pub fn view<'a>(&'a self) -> Element<'a, ConsoleMessage> {
        let log_texts = self.logs.iter().fold(column![].spacing(2), |col, log| {
            col.push(text(log).size(14))
        });

        let log_view = scrollable(log_texts).height(Length::Fill);

        container(log_view)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(10)
            .into()
    }
}
