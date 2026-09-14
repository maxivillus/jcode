use super::{RemoteConnection, Request};
use anyhow::Result;

impl RemoteConnection {
    /// Поставить в очередь обрезку контекста по явной команде пользователя.
    pub async fn context_prune(&mut self, kind: &str, keep_recent: Option<usize>) -> Result<u64> {
        self.context_prune_with_after(kind, keep_recent, None).await
    }

    /// Поставить в очередь обрезку контекста с необязательной точкой tail.
    pub async fn context_prune_with_after(
        &mut self,
        kind: &str,
        keep_recent: Option<usize>,
        after_message_id: Option<&str>,
    ) -> Result<u64> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let request = Request::ContextPrune {
            id,
            kind: kind.to_string(),
            keep_recent,
            after_message_id: after_message_id.map(str::to_string),
        };
        self.send_request(request).await?;
        Ok(id)
    }
}
