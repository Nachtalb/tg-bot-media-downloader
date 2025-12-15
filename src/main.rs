use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use teloxide::{
    net::Download,
    prelude::*,
    types::{
        FileId, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage, MessageId,
        ParseMode, ReplyParameters,
    },
};
use tokio::fs;
use tokio::sync::{Mutex, mpsc};
use tokio::time;
use url::Url;

// --- CLI Configuration ---

#[derive(Parser, Debug, Clone)]
#[clap(author, version, about = "Telegram file downloader bot")]
struct Config {
    /// Token
    #[clap(short, long)]
    token: String,

    /// Destination folder for downloaded files
    #[clap(short, long, default_value = "downloads")]
    destination: String,

    /// Use local Bot API server mode
    #[clap(long)]
    local_mode: bool,

    /// Local Bot API server URL (e.g., http://localhost:8081)
    #[clap(long, default_value = "http://localhost:8081")]
    telegram_api_server: String,

    /// Webhook URL for receiving updates (e.g., http://localhost:8433/bot)
    #[clap(long)]
    telegram_webhook_url: Option<String>,

    /// Port to listen on for webhook
    #[clap(long, default_value = "8433")]
    port: u16,

    /// Address to listen on
    #[clap(long, default_value = "127.0.0.1")]
    listen: String,
}

// --- Event Definitions ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Statistics {
    total_downloaded_count: u64,
    total_downloaded_bytes: u64,
}

impl Statistics {
    fn new() -> Self {
        Self {
            total_downloaded_count: 0,
            total_downloaded_bytes: 0,
        }
    }

    async fn load(chat_id: ChatId) -> Self {
        let filename = format!("stats_{}.json", chat_id);
        match fs::read_to_string(&filename).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| Self::new()),
            Err(_) => Self::new(),
        }
    }

    async fn save(&self, chat_id: ChatId) {
        let filename = format!("stats_{}.json", chat_id);
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(&filename, json).await;
        }
    }
}

#[derive(Debug)]
enum DownloadEvent {
    /// A new file was added to the queue
    TaskAdded(FileTask),
    /// Download started
    TaskStarted(String), // task_id
    /// Download finished successfully
    TaskDone(String), // task_id
    /// Download failed
    TaskError(String, String), // task_id, error_msg
    /// Remove task from list (sent after the 3s delay)
    TaskRemove(String), // task_id
    /// User sent a new message (we need to jump to bottom)
    UserMessageSent(MessageId),
    /// User clicked "Clear Errors"
    ClearErrors,
    /// User confirmed large file download - trigger actual download
    ConfirmLargeDownload(String, String, bool), // task_id, file_id, file_name, dest_dir, local_mode
}

#[derive(Clone, PartialEq, Debug)]
enum DownloadState {
    Queued,
    Downloading,
    Done,
    Error(String),
    AwaitingConfirmation, // Waiting for user to confirm large file download
}

#[derive(Debug)]
struct FileTask {
    id: String,
    msg_id: MessageId,
    name_display: String,
    state: DownloadState,
    link: String,
    size_bytes: u32,
    file_name: String,
    file_id: Option<FileId>,
}

// Global Map: ChatId -> Sender channel for that chat's actor
type SenderMap = Arc<Mutex<HashMap<ChatId, mpsc::Sender<DownloadEvent>>>>;

const MAX_FILE_SIZE: u32 = 20 * 1024 * 1024; // 20 MB for non-local mode
const MAX_FILE_SIZE_LOCAL: u32 = 150 * 1024 * 1024; // 150 MB for local mode (default limit)
const MAX_FILE_SIZE_LOCAL_CONFIRM: u32 = 2000 * 1024 * 1024; // 2000 MB absolute max for local mode

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("Starting Cargo Downloader Bot (Event Based)...");

    let config = Config::parse();

    // Setup bot with custom API server URL if in local mode
    let bot = if config.local_mode {
        log::info!("Running in LOCAL mode");
        log::info!("Bot API server: {}", config.telegram_api_server);
        let api_url = format!("{}/bot", config.telegram_api_server);
        Bot::new(config.token.clone())
            .set_api_url(Url::parse(&api_url).expect("Invalid telegram-api-server URL"))
    } else {
        log::info!("Running in STANDARD mode");
        Bot::new(config.token.clone())
    };

    // Create destination folder from config
    fs::create_dir_all(&config.destination)
        .await
        .expect("Could not create destination folder");

    log::info!("Downloads will be saved to: {}", config.destination);

    // Register webhook if in local mode and webhook URL is provided
    if config.local_mode
        && let Some(webhook_url) = &config.telegram_webhook_url
    {
        log::info!("Registering webhook: {}", webhook_url);
        let url = match Url::parse(webhook_url) {
            Ok(url) => url,
            Err(e) => {
                log::error!("Failed to parse webhook URL: {}", e);
                return;
            }
        };
        match bot.set_webhook(url).await {
            Ok(_) => log::info!("Webhook registered successfully"),
            Err(e) => log::error!("Failed to register webhook: {}", e),
        }
    }

    // We only store the senders. The state is owned by the spawned actors.
    let senders: SenderMap = Arc::new(Mutex::new(HashMap::new()));

    let message_handler = Update::filter_message().endpoint(file_handler);
    let callback_handler = Update::filter_callback_query().endpoint(callback_handler);

    let handler = dptree::entry()
        .branch(message_handler)
        .branch(callback_handler);

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![senders, config])
        .distribution_function(|_| None::<()>)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// --- Handlers ---

async fn file_handler(
    bot: Bot,
    msg: Message,
    senders: SenderMap,
    config: Config,
) -> ResponseResult<()> {
    // 1. Identify content
    let (file_id, file_name_prefix, file_size) = if let Some(photos) = msg.photo() {
        if let Some(p) = photos.last() {
            (p.file.id.clone(), "photo".to_string(), p.file.size)
        } else {
            return Ok(());
        }
    } else if let Some(doc) = msg.document() {
        (
            doc.file.id.clone(),
            doc.file_name.clone().unwrap_or_else(|| "doc".to_string()),
            doc.file.size,
        )
    } else if let Some(video) = msg.video() {
        (
            video.file.id.clone(),
            video
                .file_name
                .clone()
                .unwrap_or_else(|| "video".to_string()),
            video.file.size,
        )
    } else if let Some(anim) = msg.animation() {
        (
            anim.file.id.clone(),
            anim.file_name.clone().unwrap_or_else(|| "gif".to_string()),
            anim.file.size,
        )
    } else if let Some(vn) = msg.video_note() {
        (vn.file.id.clone(), "video_note".to_string(), vn.file.size)
    } else {
        return Ok(());
    };

    let chat_id = msg.chat.id;
    let msg_id = msg.id;
    let task_id = format!("{}_{}", msg_id, file_id.0);

    // Check if file already exists
    let expected_filename =
        get_expected_filename(&bot, &file_id, &file_name_prefix, &config.destination).await;
    if let Some(filename) = &expected_filename
        && fs::metadata(filename).await.is_ok()
    {
        // File already exists, skip download
        log::info!("File already exists, skipping: {}", filename);

        let _ = bot
            .send_message(
                chat_id,
                "ℹ️ <b>File already exists</b> - skipping download.",
            )
            .parse_mode(ParseMode::Html)
            .reply_parameters(ReplyParameters::new(msg_id))
            .await;

        return Ok(());
    }

    // 2. Get or Create Actor Channel
    let tx = {
        let mut map = senders.lock().await;
        if let Some(tx) = map.get(&chat_id) {
            tx.clone()
        } else {
            // Spawn new Actor
            let (tx, rx) = mpsc::channel(100);
            map.insert(chat_id, tx.clone());
            let bot_clone = bot.clone();
            let tx_for_actor = tx.clone(); // Clone for actor to use when spawning downloads
            tokio::spawn(run_ui_actor(bot_clone, chat_id, rx, tx_for_actor));
            tx
        }
    };

    // 3. Notify Actor of new user message
    let _ = tx.send(DownloadEvent::UserMessageSent(msg_id)).await;

    // 4. Validate Size
    let link_text = format!("Media #{}", msg_id);
    let message_link = build_message_link(&msg);

    // Determine max file size based on mode
    let (_max_size, needs_confirmation) = if config.local_mode {
        if file_size > MAX_FILE_SIZE_LOCAL_CONFIRM {
            // Exceeds absolute maximum
            let _ = tx
                .send(DownloadEvent::TaskAdded(FileTask {
                    id: task_id.clone(),
                    msg_id,
                    name_display: link_text.clone(),
                    state: DownloadState::Error(format!(
                        "Too large (>{})",
                        format_size(MAX_FILE_SIZE_LOCAL_CONFIRM)
                    )),
                    link: message_link.clone(),
                    size_bytes: file_size,
                    file_name: file_name_prefix.clone(),
                    file_id: None,
                }))
                .await;

            let error_text = if !message_link.is_empty() {
                format!(
                    "❌ <b>Error:</b> <a href=\"{}\">File</a> is too large ({}).\n\
                    Maximum file size in local mode is {}.",
                    message_link,
                    format_size(file_size),
                    format_size(MAX_FILE_SIZE_LOCAL_CONFIRM)
                )
            } else {
                format!(
                    "❌ <b>Error:</b> {} is too large ({}).\n\
                    Maximum file size in local mode is {}.",
                    link_text,
                    format_size(file_size),
                    format_size(MAX_FILE_SIZE_LOCAL_CONFIRM)
                )
            };

            let _ = bot
                .send_message(chat_id, error_text)
                .parse_mode(ParseMode::Html)
                .reply_parameters(ReplyParameters::new(msg_id))
                .await;

            return Ok(());
        } else if file_size > MAX_FILE_SIZE_LOCAL {
            // Requires confirmation
            (MAX_FILE_SIZE_LOCAL_CONFIRM, true)
        } else {
            // Within auto-download limit
            (MAX_FILE_SIZE_LOCAL, false)
        }
    } else {
        // Standard mode - 20MB limit
        if file_size > MAX_FILE_SIZE {
            let _ = tx
                .send(DownloadEvent::TaskAdded(FileTask {
                    id: task_id.clone(),
                    msg_id,
                    name_display: link_text.clone(),
                    state: DownloadState::Error("Too large (>20MB)".into()),
                    link: message_link.clone(),
                    size_bytes: file_size,
                    file_name: file_name_prefix.clone(),
                    file_id: None,
                }))
                .await;

            let error_text = if !message_link.is_empty() {
                format!(
                    "❌ <b>Error:</b> <a href=\"{}\">File</a> is too large ({}).",
                    message_link,
                    format_size(file_size)
                )
            } else {
                format!(
                    "❌ <b>Error:</b> {} is too large ({}).",
                    link_text,
                    format_size(file_size)
                )
            };

            let _ = bot
                .send_message(chat_id, error_text)
                .parse_mode(ParseMode::Html)
                .reply_parameters(ReplyParameters::new(msg_id))
                .await;

            return Ok(());
        }
        (MAX_FILE_SIZE, false)
    };

    // If needs confirmation, send message with button
    if needs_confirmation {
        let _ = tx
            .send(DownloadEvent::TaskAdded(FileTask {
                id: task_id.clone(),
                msg_id,
                name_display: link_text.clone(),
                state: DownloadState::AwaitingConfirmation,
                link: message_link.clone(),
                size_bytes: file_size,
                file_name: file_name_prefix.clone(),
                file_id: Some(file_id.clone()),
            }))
            .await;

        let confirm_text = if !message_link.is_empty() {
            format!(
                "⚠️ <a href=\"{}\">File</a> is large ({}).\n\
                Download may take time. Continue?",
                message_link,
                format_size(file_size)
            )
        } else {
            format!(
                "⚠️ {} is large ({}).\n\
                Download may take time. Continue?",
                link_text,
                format_size(file_size)
            )
        };

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback(
                format!("✅ Download ({})", format_size(file_size)),
                format!("confirm_download:{}", task_id),
            ),
            InlineKeyboardButton::callback("❌ Cancel", format!("cancel_download:{}", task_id)),
        ]]);

        let _ = bot
            .send_message(chat_id, confirm_text)
            .parse_mode(ParseMode::Html)
            .reply_parameters(ReplyParameters::new(msg_id))
            .reply_markup(keyboard)
            .await;

        return Ok(());
    }

    // 5. Send "Queued" Task to UI
    let _ = tx
        .send(DownloadEvent::TaskAdded(FileTask {
            id: task_id.clone(),
            msg_id,
            name_display: link_text,
            state: DownloadState::Queued,
            link: message_link,
            size_bytes: file_size,
            file_name: file_name_prefix.clone(),
            file_id: Some(file_id.clone()),
        }))
        .await;

    // 6. Spawn Download Task
    let bot_dl = bot.clone();
    let tx_dl = tx.clone();
    let dest_dir = config.destination.clone();
    let local_mode = config.local_mode;

    tokio::spawn(async move {
        let _ = tx_dl
            .send(DownloadEvent::TaskStarted(task_id.clone()))
            .await;

        match download_file_logic(&bot_dl, &file_id, &file_name_prefix, &dest_dir, local_mode).await
        {
            Ok(_) => {
                let _ = tx_dl.send(DownloadEvent::TaskDone(task_id.clone())).await;
                time::sleep(Duration::from_secs(3)).await;
                let _ = tx_dl.send(DownloadEvent::TaskRemove(task_id.clone())).await;
            }
            Err(e) => {
                let _ = tx_dl
                    .send(DownloadEvent::TaskError(task_id.clone(), e.to_string()))
                    .await;
            }
        }
    });

    Ok(())
}

async fn callback_handler(
    q: CallbackQuery,
    senders: SenderMap,
    bot: Bot,
    config: Config,
) -> ResponseResult<()> {
    if let Some(data) = &q.data {
        let chat_id = match &q.message {
            Some(MaybeInaccessibleMessage::Regular(m)) => m.chat.id,
            _ => return Ok(()),
        };

        if data == "clear_errors" {
            let map = senders.lock().await;
            if let Some(tx) = map.get(&chat_id) {
                let _ = tx.send(DownloadEvent::ClearErrors).await;
            }
            bot.answer_callback_query(q.id)
                .text("Errors cleared")
                .await?;
        } else if let Some(task_id) = data.strip_prefix("confirm_download:") {
            // User confirmed large file download
            let map = senders.lock().await;
            if let Some(tx) = map.get(&chat_id) {
                // We need to extract the file info from task_id or store it differently
                // For now, we'll send the confirmation and let the actor handle extracting from stored FileTask
                let _ = tx
                    .send(DownloadEvent::ConfirmLargeDownload(
                        task_id.to_string(),
                        config.destination.clone(),
                        config.local_mode,
                    ))
                    .await;
            }

            bot.answer_callback_query(q.id)
                .text("Download started...")
                .await?;

            // Edit the confirmation message
            if let Some(MaybeInaccessibleMessage::Regular(msg)) = &q.message {
                let _ = bot
                    .edit_message_text(chat_id, msg.id, "✅ Download confirmed and started.")
                    .await;
            }
        } else if let Some(task_id) = data.strip_prefix("cancel_download:") {
            // User cancelled large file download
            let map = senders.lock().await;
            if let Some(tx) = map.get(&chat_id) {
                let _ = tx
                    .send(DownloadEvent::TaskRemove(task_id.to_string()))
                    .await;
            }

            bot.answer_callback_query(q.id)
                .text("Download cancelled")
                .await?;

            // Edit the confirmation message
            if let Some(MaybeInaccessibleMessage::Regular(msg)) = &q.message {
                let _ = bot
                    .edit_message_text(chat_id, msg.id, "❌ Download cancelled.")
                    .await;
            }
        }
    }
    Ok(())
}

// --- The Actor ---

async fn run_ui_actor(
    bot: Bot,
    chat_id: ChatId,
    mut rx: mpsc::Receiver<DownloadEvent>,
    tx: mpsc::Sender<DownloadEvent>,
) {
    // Local State (No Mutex needed! Single thread ownership)
    let mut tasks: VecDeque<FileTask> = VecDeque::new();
    let mut status_msg_id: Option<MessageId> = None;
    let mut last_user_msg_id: MessageId = MessageId(0);

    // Load statistics from disk
    let mut stats = Statistics::load(chat_id).await;

    // Dirty flag to avoid re-rendering if nothing happened
    let mut dirty = false;

    // The ticker for UI updates (1 second)
    let mut ticker = time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            // 1. Handle incoming events (Instant State Update)
            event = rx.recv() => {
                match event {
                    Some(e) => {
                        dirty = true; // Mark needs update
                        match e {
                            DownloadEvent::TaskAdded(t) => tasks.push_back(t),
                            DownloadEvent::TaskStarted(tid) => {
                                if let Some(t) = tasks.iter_mut().find(|x| x.id == tid) {
                                    t.state = DownloadState::Downloading;
                                }
                            },
                            DownloadEvent::TaskDone(tid) => {
                                if let Some(t) = tasks.iter_mut().find(|x| x.id == tid) {
                                    t.state = DownloadState::Done;
                                    // Update statistics
                                    stats.total_downloaded_count += 1;
                                    stats.total_downloaded_bytes += t.size_bytes as u64;
                                    // Save statistics to disk
                                    stats.save(chat_id).await;
                                }
                            },
                            DownloadEvent::TaskError(tid, err) => {
                                if let Some(t) = tasks.iter_mut().find(|x| x.id == tid) {
                                    t.state = DownloadState::Error(err.clone());

                                    // Send a static error message
                                    let error_text = if !t.link.is_empty() {
                                        format!(
                                            "❌ <b>Download Failed:</b> <a href=\"{}\">{}</a>\n<b>Error:</b> {}",
                                            t.link, t.file_name, err
                                        )
                                    } else {
                                        format!(
                                            "❌ <b>Download Failed:</b> {}\n<b>Error:</b> {}",
                                            t.file_name, err
                                        )
                                    };

                                    let _ = bot
                                        .send_message(chat_id, error_text)
                                        .parse_mode(ParseMode::Html)
                                        .reply_parameters(ReplyParameters::new(t.msg_id))
                                        .await;
                                }
                            },
                            DownloadEvent::TaskRemove(tid) => {
                                // Remove if it's done OR awaiting confirmation (for cancellation)
                                if let Some(pos) = tasks.iter().position(|x| x.id == tid)
                                    && matches!(tasks[pos].state, DownloadState::Done | DownloadState::AwaitingConfirmation) {
                                        tasks.remove(pos);
                                    }
                            },
                            DownloadEvent::UserMessageSent(mid) => {
                                if mid.0 > last_user_msg_id.0 {
                                    last_user_msg_id = mid;
                                }
                            },
                            DownloadEvent::ClearErrors => {
                                tasks.retain(|t| !matches!(t.state, DownloadState::Error(_)));
                            },
                            DownloadEvent::ConfirmLargeDownload(tid, dest_dir, local_mode) => {
                                // User confirmed download - find the task and start download
                                if let Some(t) = tasks.iter_mut().find(|x| x.id == tid)
                                    && let Some(file_id) = &t.file_id {
                                        t.state = DownloadState::Queued;

                                        // Spawn the download task
                                        let bot_clone = bot.clone();
                                        let file_id_clone = file_id.clone();
                                        let file_name = t.file_name.clone();
                                        let task_id_clone = tid.clone();
                                        let tx_clone = tx.clone();

                                        tokio::spawn(async move {
                                            let _ = tx_clone
                                                .send(DownloadEvent::TaskStarted(task_id_clone.clone()))
                                                .await;

                                            match download_file_logic(&bot_clone, &file_id_clone, &file_name, &dest_dir, local_mode).await {
                                                Ok(_) => {
                                                    let _ = tx_clone.send(DownloadEvent::TaskDone(task_id_clone.clone())).await;
                                                    time::sleep(Duration::from_secs(3)).await;
                                                    let _ = tx_clone.send(DownloadEvent::TaskRemove(task_id_clone.clone())).await;
                                                }
                                                Err(e) => {
                                                    let _ = tx_clone
                                                        .send(DownloadEvent::TaskError(task_id_clone.clone(), e.to_string()))
                                                        .await;
                                                }
                                            }
                                        });
                                    }
                            }
                        }
                    }
                    None => break, // Channel closed
                }
            }

            // 2. Handle Ticker (Throttled UI Render)
            _ = ticker.tick() => {
                if dirty {
                    // Update the UI
                    update_ui(&bot, chat_id, &tasks, &mut status_msg_id, last_user_msg_id, &stats).await;
                    dirty = false;
                }
            }
        }
    }
}

async fn update_ui(
    bot: &Bot,
    chat_id: ChatId,
    tasks: &VecDeque<FileTask>,
    status_msg_id: &mut Option<MessageId>,
    last_user_msg_id: MessageId,
    stats: &Statistics,
) {
    // A. Logic: Do we need to delete the old message?
    // If the status message exists AND it is "above" the last user message, it is stale.
    let mut force_new = false;
    if let Some(sid) = *status_msg_id
        && last_user_msg_id.0 > sid.0
    {
        // It is stale. Delete it.
        let _ = bot.delete_message(chat_id, sid).await;
        *status_msg_id = None;
        force_new = true;
    }

    // B. Generate Text
    let (text, keyboard) = generate_status_text(tasks, stats);

    // C. Send or Edit
    if let Some(sid) = *status_msg_id {
        if !force_new {
            // Edit existing
            let mut req = bot
                .edit_message_text(chat_id, sid, &text)
                .parse_mode(ParseMode::Html);
            if let Some(kb) = keyboard {
                req = req.reply_markup(kb);
            }
            // Note: If text didn't change, Telegram returns error. We ignore it.
            if (req.await).is_err() {
                // If edit fails (e.g. user deleted msg), reset ID so we send new next time
                *status_msg_id = None;
            }
        } else {
            // Fallthrough to send new
            send_new(bot, chat_id, text, keyboard, status_msg_id).await;
        }
    } else {
        // Send new
        send_new(bot, chat_id, text, keyboard, status_msg_id).await;
    }
}

async fn send_new(
    bot: &Bot,
    chat_id: ChatId,
    text: String,
    kb: Option<InlineKeyboardMarkup>,
    status_msg_id: &mut Option<MessageId>,
) {
    let mut req = bot.send_message(chat_id, text).parse_mode(ParseMode::Html);
    if let Some(k) = kb {
        req = req.reply_markup(k);
    }
    match req.await {
        Ok(m) => *status_msg_id = Some(m.id),
        Err(e) => log::error!("Failed to send status: {}", e),
    }
}

// --- Helpers ---

fn build_message_link(msg: &Message) -> String {
    // Try to get the public URL first (for groups/channels)
    if let Some(url) = msg.url() {
        return url.to_string();
    }

    // For private chats, build a tg:// link using chat_id
    // In private chats, chat.id is the same as the user's id
    format!(
        "tg://openmessage?user_id={}&message_id={}",
        msg.chat.id, msg.id
    )
}

fn format_size(size: u32) -> String {
    let mb = size as f64 / 1024.0 / 1024.0;
    if mb >= 1.0 {
        format!("{:.1}MB", mb)
    } else {
        let kb = size as f64 / 1024.0;
        format!("{:.0}KB", kb)
    }
}

fn format_size_u64(size: u64) -> String {
    let gb = size as f64 / 1024.0 / 1024.0 / 1024.0;
    if gb >= 1.0 {
        format!("{:.2}GB", gb)
    } else {
        let mb = size as f64 / 1024.0 / 1024.0;
        if mb >= 1.0 {
            format!("{:.1}MB", mb)
        } else {
            let kb = size as f64 / 1024.0;
            format!("{:.0}KB", kb)
        }
    }
}

fn generate_status_text(
    tasks: &VecDeque<FileTask>,
    stats: &Statistics,
) -> (String, Option<InlineKeyboardMarkup>) {
    if tasks.is_empty() {
        let mut text =
            String::from("<b>📂 Download Queue</b>\n\n<i>Queue is empty. Waiting for files...</i>");

        // Show statistics even when queue is empty
        if stats.total_downloaded_count > 0 {
            text.push_str(&format!(
                "\n\n━━━━━━━━━━━━━━━━━━\n📊 <b>Total:</b> {} files | {}",
                stats.total_downloaded_count,
                format_size_u64(stats.total_downloaded_bytes)
            ));
        }

        return (text, None);
    }

    let mut text = String::from("<b>📂 Download Queue</b>\n\n");
    let mut has_errors = false;
    let max_lines = 8;

    let downloading_count = tasks
        .iter()
        .filter(|t| {
            matches!(
                t.state,
                DownloadState::Downloading
                    | DownloadState::Queued
                    | DownloadState::AwaitingConfirmation
            )
        })
        .count();
    let done_count = tasks
        .iter()
        .filter(|t| matches!(t.state, DownloadState::Done))
        .count();

    for (i, task) in tasks.iter().enumerate() {
        if i >= max_lines {
            text.push_str(&format!(
                "\n<i>... and {} more</i>",
                tasks.len() - max_lines
            ));
            break;
        }

        let size_str = format_size(task.size_bytes);

        // Always create a link if we have a message link
        let link_html = if !task.link.is_empty() {
            format!("<a href=\"{}\">{}</a>", task.link, task.name_display)
        } else {
            task.name_display.clone()
        };

        match &task.state {
            DownloadState::Queued => {
                text.push_str(&format!(
                    "⏳ <code>[{}]</code> {} (Queued)\n",
                    size_str, link_html
                ));
            }
            DownloadState::Downloading => {
                text.push_str(&format!(
                    "⬇️ <code>[{}]</code> {} ...\n",
                    size_str, link_html
                ));
            }
            DownloadState::Done => {
                text.push_str(&format!("✅ <code>[{}]</code> {} \n", size_str, link_html));
            }
            DownloadState::AwaitingConfirmation => {
                text.push_str(&format!(
                    "⚠️ <code>[{}]</code> {} (Awaiting confirmation)\n",
                    size_str, link_html
                ));
            }
            DownloadState::Error(err_msg) => {
                has_errors = true;
                // Show error message in the status
                text.push_str(&format!(
                    "❌ <code>[{}]</code> {} \n   <i>{}</i>\n",
                    size_str, link_html, err_msg
                ));
            }
        }
    }

    text.push_str("\n━━━━━━━━━━━━━━━━━━\n");
    text.push_str(&format!(
        "📊 Active: {} | Done: {}\n",
        downloading_count, done_count
    ));
    text.push_str(&format!(
        "📈 <b>Total:</b> {} files | {}",
        stats.total_downloaded_count,
        format_size_u64(stats.total_downloaded_bytes)
    ));

    let keyboard = if has_errors {
        let btn = InlineKeyboardButton::callback("Clear Errors 🗑️", "clear_errors");
        Some(InlineKeyboardMarkup::new(vec![vec![btn]]))
    } else {
        None
    };

    (text, keyboard)
}

async fn get_expected_filename(
    bot: &Bot,
    file_id: &FileId,
    name_prefix: &str,
    dest_dir: &str,
) -> Option<String> {
    match bot.get_file(file_id.clone()).await {
        Ok(file) => {
            let extension = Path::new(&file.path)
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("");
            let file_name = format!("{}_{}.{}", name_prefix, file_id.0, extension);
            Some(format!("{}/{}", dest_dir, file_name))
        }
        Err(_) => None,
    }
}

async fn download_file_logic(
    bot: &Bot,
    file_id: &FileId,
    name_prefix: &str,
    dest_dir: &str,
    local_mode: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(file_id.clone()).await?;
    let file_path = file.path.clone();
    let extension = Path::new(&file_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");

    let file_name = format!("{}_{}.{}", name_prefix, file_id.0, extension);
    let destination = format!("{}/{}", dest_dir, file_name);

    if local_mode {
        // In local mode, file.path is an absolute path on the local filesystem
        // We can just move/copy the file instead of downloading
        log::info!(
            "Local mode: Moving file from {} to {}",
            file_path,
            destination
        );

        // Try to move first (faster), fall back to copy if move fails
        match fs::rename(&file_path, &destination).await {
            Ok(_) => {
                log::info!("File moved successfully");
                Ok(())
            }
            Err(e) => {
                log::warn!("Move failed ({}), trying copy instead", e);
                // If move fails (e.g., across filesystems), try copy
                fs::copy(&file_path, &destination).await?;
                // Optionally delete the original
                let _ = fs::remove_file(&file_path).await;
                Ok(())
            }
        }
    } else {
        // Standard mode: download from Telegram servers
        let mut dest_file = fs::File::create(&destination).await?;
        bot.download_file(&file.path, &mut dest_file).await?;
        Ok(())
    }
}
