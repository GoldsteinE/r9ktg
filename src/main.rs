use std::{borrow::Cow, fmt, future::Future, path::PathBuf, sync::Arc};

use color_eyre::eyre;
use serde::Deserialize;
use size_format::SizeFormatterBinary;
use sled::CompareAndSwapError;
use teloxide::{
    dispatching::UpdateFilterExt,
    dptree,
    net::Download,
    payloads::SendMessageSetters as _,
    prelude::{Dispatcher, Request as _, Requester as _},
    types::{
        Chat, ChatId, Document, MediaDocument, MediaKind, MediaText, Message, MessageCommon,
        MessageKind, Update, User,
    },
    Bot,
};
use tracing_futures::Instrument as _;
use tracing_subscriber::EnvFilter;
use xxhash_rust::xxh3::Xxh3;

#[derive(Deserialize)]
#[serde(transparent)]
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    token: Token,
    db_path: PathBuf,
    #[serde(default = "default_max_import_size")]
    max_import_size: u32,
    #[serde(default)]
    allow_duplicates_in_replies: bool,
}

fn default_max_import_size() -> u32 {
    50 * 1024 * 1024
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImportTextChunk<'a> {
    Simple(#[serde(borrow)] Cow<'a, str>),
    Typed {
        #[serde(borrow)]
        text: Cow<'a, str>,
    },
}

impl ImportTextChunk<'_> {
    fn as_str(&self) -> &str {
        match self {
            ImportTextChunk::Simple(text) | ImportTextChunk::Typed { text } => text.as_ref(),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImportText<'a> {
    Simple(#[serde(borrow)] Cow<'a, str>),
    Chunked(#[serde(borrow)] Vec<ImportTextChunk<'a>>),
}

impl<'a> ImportText<'a> {
    fn moo(self) -> Cow<'a, str> {
        match self {
            ImportText::Simple(cow) => cow,
            ImportText::Chunked(chunks) => chunks.iter().map(ImportTextChunk::as_str).collect(),
        }
    }
}

#[derive(Deserialize)]
struct ImportMessage<'a> {
    #[serde(borrow)]
    r#type: Cow<'a, str>,
    #[serde(borrow)]
    text: ImportText<'a>,
}

#[derive(Deserialize)]
struct Import<'a> {
    #[serde(borrow)]
    messages: Vec<ImportMessage<'a>>,
}

#[derive(Clone)]
struct Robot9000 {
    db: sled::Db,
    hasher: Box<Xxh3>,
    config: Arc<Config>,
}

impl Robot9000 {
    fn hash_message(&mut self, chat_id: ChatId, text: impl AsRef<[u8]>) -> [u8; 16] {
        self.hasher.reset();
        self.hasher.update(&chat_id.0.to_le_bytes());
        self.hasher.update(text.as_ref());
        self.hasher.digest128().to_le_bytes()
    }

    fn store_message(&mut self, chat_id: ChatId, text: impl AsRef<[u8]>) -> eyre::Result<bool> {
        let hash = self.hash_message(chat_id, text);
        match self.db.compare_and_swap(&hash, None::<&[u8]>, Some(&[]))? {
            Err(CompareAndSwapError {
                current: Some(current),
                ..
            }) => Ok(current.len() == 0),
            _ => Ok(false),
        }
    }

    fn allow_message(&mut self, chat_id: ChatId, text: impl AsRef<[u8]>) -> eyre::Result<()> {
        let hash = self.hash_message(chat_id, text);
        self.db.insert(&hash, &[1])?;
        Ok(())
    }

    fn forbid_message(&mut self, chat_id: ChatId, text: impl AsRef<[u8]>) -> eyre::Result<()> {
        let hash = self.hash_message(chat_id, text);
        self.db.insert(&hash, &[])?;
        Ok(())
    }

    async fn is_admin(bot: &Bot, chat: &Chat, user: &User) -> eyre::Result<bool> {
        Ok(chat.is_private()
            || bot
                .get_chat_member(chat.id, user.id)
                .send()
                .await?
                .can_delete_messages())
    }

    async fn ensure_admin<Fut>(
        bot: &Bot,
        message: &Message,
        user: &User,
        f: Fut,
    ) -> eyre::Result<()>
    where
        Fut: Future<Output = eyre::Result<()>>,
    {
        if !Self::is_admin(bot, &message.chat, user).await? {
            tracing::info!(user_id = user.id.0, "someone tried to run admin command");
            bot.send_message(message.chat.id, "Nice try")
                .reply_to_message_id(message.id)
                .send()
                .await?;
            Ok(())
        } else {
            f.await
        }
    }

    async fn import_document(
        &mut self,
        bot: &Bot,
        user: &User,
        message: &Message,
        document: &Document,
    ) -> eyre::Result<()> {
        if document.file_size > self.config.max_import_size {
            tracing::info!(
                user_id = user.id.0,
                file_size = document.file_size,
                max_import_size = self.config.max_import_size,
                "/import failed due to file size",
            );
            let reply = format!(
                "Come on, there's no way I'll import a {}B file (my limit is {}B)",
                SizeFormatterBinary::new(document.file_size.into()),
                SizeFormatterBinary::new(self.config.max_import_size.into()),
            );
            bot.send_message(message.chat.id, reply)
                .reply_to_message_id(message.id)
                .send()
                .await?;
            return Ok(());
        }

        let mut file = Vec::with_capacity(document.file_size as usize);
        let file_info = bot.get_file(&document.file_id).send().await?;
        bot.download_file(&file_info.file_path, &mut file).await?;
        match serde_json::from_slice::<Import>(&*file) {
            Ok(import) => {
                let imported_count = import
                    .messages
                    .into_iter()
                    .filter_map(|import_message| {
                        (import_message.r#type == "message").then(|| {
                            self.store_message(message.chat.id, &*import_message.text.moo())
                                .map(|b| usize::from(!b))
                        })
                    })
                    .sum::<Result<usize, _>>()?;
                tracing::info!(
                    user_id = user.id.0,
                    count = imported_count,
                    "/import succeeded"
                );

                let reply = format!(
                    "Sucessfully imported {imported_count} messages (excluding duplicates)"
                );
                bot.send_message(message.chat.id, reply)
                    .reply_to_message_id(message.id)
                    .send()
                    .await?;
            }
            Err(err) => {
                tracing::info!(
                    user_id = user.id.0,
                    err = format_args!("{err}"),
                    "/import failed due to deserialization error",
                );
                let reply = format!("Failed to parse your import, sorry :(\nError: {err}");
                bot.send_message(message.chat.id, reply)
                    .reply_to_message_id(message.id)
                    .send()
                    .await?;
            }
        }

        Ok(())
    }

    async fn reply_command(
        &mut self,
        bot: &Bot,
        message: &Message,
        reply_to: &Message,
        user: &User,
        text: &str,
    ) -> eyre::Result<bool> {
        let reply_to_text = match &reply_to.kind {
            MessageKind::Common(MessageCommon {
                media_kind: MediaKind::Text(MediaText { text, .. }),
                ..
            }) => text,
            _ => return Ok(false),
        };

        match text.trim() {
            "/allow" => {
                tracing::info!(allowed_message_id = reply_to.id, "allowed message");
                Self::ensure_admin(bot, message, user, async {
                    self.allow_message(message.chat.id, reply_to_text)
                })
                .await?;
                Ok(true)
            }
            "/forbid" => {
                tracing::info!(allowed_message_id = reply_to.id, "forbade message");
                Self::ensure_admin(bot, message, user, async {
                    self.forbid_message(message.chat.id, reply_to_text)
                })
                .await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn process_message(&mut self, message: Message, bot: Bot) -> eyre::Result<()> {
        if let MessageKind::Common(
            kind @ MessageCommon {
                from: Some(user),
                reply_to_message,
                ..
            },
        ) = &message.kind
        {
            match &kind.media_kind {
                MediaKind::Text(text) => {
                    if let Some(reply_to) = reply_to_message {
                        if self
                            .reply_command(&bot, &message, reply_to, user, &text.text)
                            .await?
                        {
                            return Ok(());
                        }

                        if self.config.allow_duplicates_in_replies {
                            return Ok(());
                        }
                    }

                    if self.store_message(message.chat.id, &text.text)? {
                        tracing::debug!(
                            text = format_args!("{:?}", text.text),
                            "deleted duplicate message"
                        );
                        bot.delete_message(message.chat.id, message.id)
                            .send()
                            .await?;
                    } else {
                        tracing::debug!(
                            text = format_args!("{:?}", text.text),
                            "ignoring unique message"
                        );
                    }
                }
                MediaKind::Document(MediaDocument {
                    document,
                    caption: Some(caption),
                    ..
                }) => {
                    if caption.trim() == "/import" {
                        Self::ensure_admin(
                            &bot,
                            &message,
                            user,
                            self.import_document(&bot, user, &message, document),
                        )
                        .await?;
                    }
                }
                _ => (),
            }
        }

        Ok(())
    }
}

async fn process_message_free(
    message: Message,
    bot: Bot,
    mut robot: Robot9000,
) -> eyre::Result<()> {
    let span = tracing::info_span!(
        "message",
        chat_id = message.chat.id.0,
        id = message.id,
        date = format_args!("{:?}", message.date),
    );
    robot.process_message(message, bot).instrument(span).await
}

async fn do_main() -> eyre::Result<()> {
    let config: Config = envy::prefixed("R9KTG_").from_env()?;
    tracing::info!(
        config = format_args!("{config:?}"),
        "Starting R9K Telegram bot"
    );

    let bot = Bot::new(&config.token.0);
    let db = sled::open(&config.db_path)?;
    tracing::debug!("Opened database");
    let hasher = Box::new(Xxh3::new());
    let robot = Robot9000 {
        db,
        hasher,
        config: Arc::new(config),
    };

    Dispatcher::builder(
        bot,
        Update::filter_message().chain(dptree::endpoint(process_message_free)),
    )
    .enable_ctrlc_handler()
    .dependencies(dptree::deps![robot])
    .build()
    .dispatch()
    .await;

    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    do_main().await
}
