use serenity::{
    async_trait,
    framework::standard:: {
        macros::hook, CommandResult, DispatchError
    },
    model::{
        channel::Message,
        guild::{Guild, GuildUnavailable},
        id::{ChannelId, MessageId},
        gateway::Ready
    },
    prelude::*,
};

use chrono::{DateTime, Duration, Utc};

use crate::cache::*;
use crate::utls::discordhelpers;
use crate::managers::stats::StatsManager;
use serenity::model::id::{GuildId};
use serenity::model::event::{MessageUpdateEvent};
use crate::utls::discordhelpers::embeds;
use tokio::sync::MutexGuard;
use serenity::model::channel::{ReactionType};

use crate::utls::parser::{get_message_attachment, shortname_to_qualified};
use crate::managers::compilation::RequestHandler;
use serenity::collector::CollectReaction;
use crate::commands::compile::handle_request;
use crate::utls::discordhelpers::embeds::embed_message;

pub struct Handler; // event handler for serenity

#[async_trait]
trait ShardsReadyHandler {
    async fn all_shards_ready(&self, ctx: &Context, stats: & mut MutexGuard<'_, StatsManager>, ready : &Ready);
}

#[async_trait]
impl ShardsReadyHandler for Handler {
    async fn all_shards_ready(&self, ctx: &Context, stats: & mut MutexGuard<'_, StatsManager>, ready : &Ready) {
        let data = ctx.data.read().await;
        let mut info = data.get::<ConfigCache>().unwrap().write().await;
        info.insert("BOT_AVATAR", ready.user.avatar_url().unwrap());

        let shard_manager = data.get::<ShardManagerCache>().unwrap().lock().await;
        let guild_count = stats.get_boot_vec_sum();

        // update stats
        if stats.should_track() {
            stats.post_servers(guild_count).await;
        }

        discordhelpers::send_global_presence(&shard_manager, stats.server_count()).await;

        info!("Ready in {} guilds", stats.server_count());
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message_update(&self, ctx: Context, new_data: MessageUpdateEvent) {
        let old_msg = {
            let data = ctx.data.read().await;
            let mut message_cache = data.get::<MessageCache>().unwrap().lock().await;
            if let Some(msg) = message_cache.get_mut(&new_data.id.0) {
                Some(msg.clone())
            }
            else {
                None
            }
        };

        if let Some(msg) = old_msg {
            if let Some(new_msg) = new_data.content {
                if let Some (author) = new_data.author {
                    discordhelpers::handle_edit(&ctx, new_msg, author, msg).await;
                }
            }
        }
    }

    async fn guild_create(&self, ctx: Context, guild: Guild) {
        let now: DateTime<Utc> = Utc::now();
        if guild.joined_at + Duration::seconds(30) > now {
            let data = ctx.data.read().await;

            // post new server to join log
            let id;
            {
                let info = data.get::<ConfigCache>().unwrap().read().await;
                id = info.get("BOT_ID").unwrap().parse::<u64>().unwrap();

                if let Some(log) = info.get("JOIN_LOG") {
                    if let Ok(id) = log.parse::<u64>() {
                        let emb = embeds::build_join_embed(&guild);
                        discordhelpers::manual_dispatch(ctx.http.clone(), id, emb).await;
                    }
                }
            }

            // publish/queue new server to stats
            let mut stats = data.get::<StatsManagerCache>().unwrap().lock().await;
            if stats.should_track() {
                stats.new_server().await;
            }

            // ensure we're actually loaded in before we start posting our server counts
            if stats.server_count() > 0
            {
                let new_stats = dbl::types::ShardStats::Cumulative {
                    server_count: stats.server_count(),
                    shard_count: Some(stats.shard_count())
                };

                let dbl = data.get::<DblCache>().unwrap().read().await;
                if let Err(e) = dbl.update_stats(id, new_stats).await {
                    warn!("Failed to post stats to dbl: {}", e);
                }

                // update guild count in presence
                let shard_manager = data.get::<ShardManagerCache>().unwrap().lock().await;
                discordhelpers::send_global_presence(&shard_manager, stats.server_count()).await;
            }

            info!("Joining {}", guild.name);

            if let Some(system_channel) = guild.system_channel_id {
                let mut message = embeds::embed_message(embeds::build_welcome_embed());
                let _ = system_channel.send_message(&ctx.http, |_| &mut message).await;
            }
            else {
                for (_, channel) in guild.channels {
                    if channel.name.contains("general") {
                        let mut message = embeds::embed_message(embeds::build_welcome_embed());
                        let _ = channel.send_message(&ctx.http, |_| &mut message).await;
                    }
                }
            }
        }
    }

    async fn message_delete(&self, ctx: Context, _channel_id: ChannelId, id: MessageId, _guild_id: Option<GuildId>) {
        let data = ctx.data.read().await;
        let mut message_cache = data.get::<MessageCache>().unwrap().lock().await;
        if let Some(msg) = message_cache.get_mut(id.as_u64()) {
            if msg.delete(ctx.http).await.is_err() {
                // ignore for now
            }
            message_cache.remove(id.as_u64());
        }
    }

    async fn guild_delete(&self, ctx: Context, incomplete: GuildUnavailable) {
        let data = ctx.data.read().await;

        // post new server to join log
        let info = data.get::<ConfigCache>().unwrap().read().await;
        let id = info.get("BOT_ID").unwrap().parse::<u64>().unwrap();
        if let Some(log) = info.get("JOIN_LOG") {
            if let Ok(id) = log.parse::<u64>() {
                let emb = embeds::build_leave_embed(&incomplete.id);
                discordhelpers::manual_dispatch(ctx.http.clone(), id, emb).await;
            }
        }

        // publish/queue new server to stats
        let mut stats = data.get::<StatsManagerCache>().unwrap().lock().await;
        if stats.should_track() {
            stats.leave_server().await;
        }

        // ensure we're actually loaded in before we start posting our server counts
        if stats.server_count() > 0
        {
            let new_stats = dbl::types::ShardStats::Cumulative {
                server_count: stats.server_count(),
                shard_count: Some(stats.shard_count())
            };

            let dbl = data.get::<DblCache>().unwrap().read().await;
            if let Err(e) = dbl.update_stats(id, new_stats).await {
                warn!("Failed to post stats to dbl: {}", e);
            }

            // update guild count in presence
            let shard_manager = data.get::<ShardManagerCache>().unwrap().lock().await;
            discordhelpers::send_global_presence(&shard_manager, stats.server_count()).await;
        }

        info!("Leaving {}", &incomplete.id);
    }

    async fn message(&self, ctx: Context, new_message: Message) {
        if !new_message.attachments.is_empty() {
            if let Ok((code, language)) = get_message_attachment(&new_message.attachments).await {
                let data = ctx.data.read().await;
                let target = {
                    let cm = data.get::<CompilerCache>().unwrap().read().await;
                    cm.resolve_target(shortname_to_qualified(&language))
                };

                if !matches!(target,  RequestHandler::None) {
                    let reaction = {
                        let botinfo = data.get::<ConfigCache>().unwrap().read().await;
                        if let Some(id) = botinfo.get("LOGO_EMOJI_ID") {
                            let name = botinfo.get("LOGO_EMOJI_NAME").expect("Unable to find loading emoji name").clone();
                            discordhelpers::build_reaction(id.parse::<u64>().unwrap(), &name)
                        }
                        else {
                            ReactionType::Unicode(String::from("💻"))
                        }
                    };

                    if let Err(_) = new_message.react(&ctx.http, reaction.clone()).await {
                        return;
                    }

                    let collector = CollectReaction::new(ctx.clone())
                        .message_id(new_message.id)
                        .timeout(core::time::Duration::new(30, 0))
                        .filter(move |r| r.emoji.eq(&reaction)).await;
                    let _ = new_message.delete_reactions(&ctx.http).await;
                    if let Some(_) = collector {
                        let emb = match handle_request(ctx.clone(), format!(";compile\n```{}\n{}\n```", language, code), new_message.author.clone(), &new_message).await {
                            Ok(emb) => emb,
                            Err(e) => {
                                let emb = embeds::build_fail_embed(&new_message.author, &format!("{}", e));
                                let mut emb_msg = embeds::embed_message(emb);
                                if let Ok(sent) = new_message
                                    .channel_id
                                    .send_message(&ctx.http, |_| &mut emb_msg)
                                    .await
                                {
                                    let mut message_cache = data.get::<MessageCache>().unwrap().lock().await;
                                    message_cache.insert(new_message.id.0, sent);
                                }
                                return;
                            }
                        };
                        let mut emb_msg = embed_message(emb);
                        emb_msg.reference_message(&new_message);
                        let _= new_message
                            .channel_id
                            .send_message(&ctx.http, |_| &mut emb_msg)
                            .await;

                    }
                }
            }
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("[Shard {}] Ready", ctx.shard_id);

        let data = ctx.data.read().await;
        let mut stats = data.get::<StatsManagerCache>().unwrap().lock().await;

        // occasionally we can have a ready event fire well after execution
        // this check prevents us from double calling all_shards_ready
        let total_shards_to_spawn = ready.shard.unwrap()[1];
        if stats.shard_count()+1 > total_shards_to_spawn {
            info!("Skipping duplicate ready event...");
            return;
        }

        let guild_count = ready.guilds.len() as u64;
        stats.add_shard(guild_count);

        if stats.shard_count() == total_shards_to_spawn {
            self.all_shards_ready(&ctx, & mut stats, &ready).await;
        }
    }
}

#[hook]
pub async fn before(ctx: &Context, msg : &Message, _: &str) -> bool {
    let data = ctx.data.read().await;
    {
        let stats = data.get::<StatsManagerCache>().unwrap().lock().await;
        if stats.should_track() {
            stats.post_request().await;
        }
    }

    // we'll go with 0 if we couldn't grab guild id
    let mut guild_id = 0;
    if let Some(id) = msg.guild_id {
        guild_id = id.0;
    }

    // check user against our blocklist
    {
        let blocklist = data.get::<BlocklistCache>().unwrap().read().await;
        let author_blocklisted = blocklist.contains(msg.author.id.0);
        let guild_blocklisted = blocklist.contains(guild_id);

        if author_blocklisted || guild_blocklisted {
            let emb = embeds::build_fail_embed(&msg.author,
       "This server or your user is blocked from executing commands.
            This may have happened due to abuse, spam, or other reasons.
            If you feel that this has been done in error, request an unban in the support server.");

            let mut emb_msg = embeds::embed_message(emb);
            if msg.channel_id.send_message(&ctx.http, |_| &mut emb_msg).await.is_ok() {
                if author_blocklisted {
                    warn!("Blocked user {} [{}]", msg.author.tag(), msg.author.id.0);
                }
                else {
                    warn!("Blocked guild {}", guild_id);
                }
            }
            return false;
        }
    }

    true
}

#[hook]
pub async fn after(
    ctx: &Context,
    msg: &Message,
    command_name: &str,
    command_result: CommandResult,
) {
    let data = ctx.data.read().await;

    if let Err(e) = command_result {
        let emb = embeds::build_fail_embed(&msg.author, &format!("{}", e));
        let mut emb_msg = embeds::embed_message(emb);
        if let Ok(sent) = msg
            .channel_id
            .send_message(&ctx.http, |_| &mut emb_msg)
            .await
        {
            let mut message_cache = data.get::<MessageCache>().unwrap().lock().await;
            message_cache.insert(msg.id.0, sent);
        }
    }


    // push command executed to api
    let stats = data.get::<StatsManagerCache>().unwrap().lock().await;
    if stats.should_track() {
        stats.command_executed(command_name, msg.guild_id).await;
    }
}

#[hook]
pub async fn dispatch_error(ctx: &Context, msg: &Message, error: DispatchError) {
    if let DispatchError::Ratelimited(_) = error {
        let emb =
            embeds::build_fail_embed(&msg.author, "You are sending requests too fast!");
        let mut emb_msg = embeds::embed_message(emb);
        if msg
            .channel_id
            .send_message(&ctx.http, |_| &mut emb_msg)
            .await
            .is_err()
        {}
    }
}
