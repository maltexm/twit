use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use reqwest::Method;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::logging;
use crate::twitter::auth::{self, OAuthApp, Token};
use crate::twitter::types::{Media, MediaType, Tweet, UrlEntity, User};

/// Browser user agent for fetching images from pbs.twimg.com (not used for the API).
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const API_BASE: &str = "https://api.x.com";
const TWEET_FIELDS: &str = "created_at,public_metrics,conversation_id,in_reply_to_user_id,lang,entities,referenced_tweets,attachments,note_tweet,author_id";
const EXPANSIONS: &str = "author_id,attachments.media_keys,referenced_tweets.id,referenced_tweets.id.author_id,in_reply_to_user_id";
const USER_FIELDS: &str = "name,username,profile_image_url,verified,verified_type";
const MEDIA_FIELDS: &str = "type,url,preview_image_url,width,height,variants";

pub struct TwitterClient {
    http: reqwest::Client,
    app: OAuthApp,
    token: Mutex<Token>,
}

#[derive(Debug, Clone)]
pub struct TimelinePage {
    pub tweets: Vec<Tweet>,
    pub next_cursor: Option<String>,
}

/// A user from the following list
#[derive(Debug, Clone)]
pub struct FollowingUser {
    pub id: String,
    pub screen_name: String,
    pub name: String,
}

/// A page of following results
#[derive(Debug)]
pub struct FollowingPage {
    pub users: Vec<FollowingUser>,
    pub next_cursor: Option<String>,
}

fn build_http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("twit/", env!("CARGO_PKG_VERSION")))
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .https_only(true)
        .build()?)
}

/// Run the OAuth browser flow, look up the user, persist token.json; returns the @username.
pub async fn login(app: OAuthApp) -> Result<String> {
    let http = build_http()?;
    let token = auth::authorize(&http, &app).await?;
    let client = TwitterClient {
        http,
        app,
        token: Mutex::new(token),
    };
    let me = client
        .request(Method::GET, "/2/users/me", &[], None)
        .await?;
    let data = me.get("data").context("users/me returned no data")?;
    let mut token = client.token.lock().await;
    token.user_id = str_at(data, "id").to_string();
    token.username = str_at(data, "username").to_string();
    token.save()?;
    Ok(token.username.clone())
}

impl TwitterClient {
    /// Load the stored token; fails with a "run `twit auth`" hint if there is none.
    pub fn new(
        client_id: String,
        client_secret: Option<String>,
        redirect_uri: String,
    ) -> Result<Self> {
        Ok(Self {
            http: build_http()?,
            app: OAuthApp {
                client_id,
                client_secret,
                redirect_uri,
            },
            token: Mutex::new(Token::load()?),
        })
    }

    /// Current access token; refreshes (and persists) it when forced or about to expire.
    async fn access_token(&self, force_refresh: bool) -> Result<String> {
        let mut token = self.token.lock().await;
        if force_refresh || token.expires_within(60) {
            auth::refresh(&self.http, &self.app, &mut token)
                .await
                .map_err(|e| {
                    logging::log_error("twitter_auth", &format!("Refresh failed: {e:#}"));
                    anyhow::anyhow!("Token refresh failed ({e}). Run `twit auth` to sign in again.")
                })?;
            token.save()?;
        }
        Ok(token.access_token.clone())
    }

    async fn user_id(&self) -> String {
        self.token.lock().await.user_id.clone()
    }

    /// Authenticated API call; refreshes once and retries on 401.
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut retried = false;
        loop {
            let access = self.access_token(retried).await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{API_BASE}{path}"))
                .bearer_auth(access)
                .query(query);
            if let Some(body) = &body {
                req = req.json(body);
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && !retried {
                retried = true;
                continue;
            }
            if status.is_success() {
                return resp.json().await.context("Failed to parse JSON response");
            }
            let reset = resp
                .headers()
                .get("x-rate-limit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
                .and_then(|ts| DateTime::from_timestamp(ts, 0))
                .map(|t| t.with_timezone(&Local).format("%H:%M:%S").to_string());
            let text = resp.text().await.unwrap_or_default();
            logging::log_error("twitter_api", &format!("{path} status {status}: {text}"));
            if status.as_u16() == 429 {
                anyhow::bail!(
                    "X API rate limit hit (HTTP 429); resets at {}",
                    reset.as_deref().unwrap_or("unknown time")
                );
            }
            let err: Value = serde_json::from_str(&text).unwrap_or_default();
            let detail = [str_at(&err, "title"), str_at(&err, "detail")]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            if detail.is_empty() {
                anyhow::bail!("X API request failed (HTTP {})", status.as_u16());
            }
            anyhow::bail!("X API error (HTTP {}): {detail}", status.as_u16());
        }
    }

    /// Fetch home timeline
    pub async fn get_home_timeline(&self, count: usize) -> Result<Vec<Tweet>> {
        Ok(self.get_home_timeline_page(count, None).await?.tweets)
    }

    /// Fetch home timeline page with optional cursor (pagination_token)
    pub async fn get_home_timeline_page(
        &self,
        count: usize,
        cursor: Option<&str>,
    ) -> Result<TimelinePage> {
        let body = self.fetch_timeline(count, cursor).await?;
        Ok(parse_timeline(&body))
    }

    async fn fetch_timeline(&self, count: usize, cursor: Option<&str>) -> Result<Value> {
        let me = self.user_id().await;
        let mut query = vec![
            ("max_results", count.clamp(1, 100).to_string()),
            ("tweet.fields", TWEET_FIELDS.to_string()),
            ("expansions", EXPANSIONS.to_string()),
            ("user.fields", USER_FIELDS.to_string()),
            ("media.fields", MEDIA_FIELDS.to_string()),
        ];
        if let Some(c) = cursor {
            query.push(("pagination_token", c.to_string()));
        }
        self.request(
            Method::GET,
            &format!("/2/users/{me}/timelines/reverse_chronological"),
            &query,
            None,
        )
        .await
    }

    /// Fetch raw home timeline JSON for debugging
    pub async fn get_home_timeline_raw(&self, count: usize) -> Result<Value> {
        self.fetch_timeline(count, None).await
    }

    /// Look up a user's ID by their screen name
    pub async fn get_user_id_by_screen_name(&self, screen_name: &str) -> Result<String> {
        let body = self
            .request(
                Method::GET,
                &format!("/2/users/by/username/{}", urlencoding::encode(screen_name)),
                &[],
                None,
            )
            .await?;
        body.pointer("/data/id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .context("User not found or invalid response")
    }

    /// Get a page of accounts that a user is following
    pub async fn get_following_page(
        &self,
        user_id: &str,
        cursor: Option<&str>,
    ) -> Result<FollowingPage> {
        let mut query = vec![
            ("max_results", "1000".to_string()),
            ("user.fields", "name,username".to_string()),
        ];
        if let Some(c) = cursor {
            query.push(("pagination_token", c.to_string()));
        }
        let body = self
            .request(
                Method::GET,
                &format!("/2/users/{user_id}/following"),
                &query,
                None,
            )
            .await?;
        let users = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .map(|u| FollowingUser {
                        id: str_at(u, "id").to_string(),
                        screen_name: str_at(u, "username").to_string(),
                        name: str_at(u, "name").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(FollowingPage {
            users,
            next_cursor: next_token(&body),
        })
    }

    /// Follow a user by their ID
    pub async fn follow_user(&self, user_id: &str) -> Result<()> {
        let me = self.user_id().await;
        self.request(
            Method::POST,
            &format!("/2/users/{me}/following"),
            &[],
            Some(serde_json::json!({ "target_user_id": user_id })),
        )
        .await
        .map(|_| ())
    }
}

// ---- v2 response mapping -------------------------------------------------

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn next_token(body: &Value) -> Option<String> {
    body.pointer("/meta/next_token")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// `includes.<kind>` indexed by `id_key`.
fn index<'a>(body: &'a Value, kind: &str, id_key: &str) -> HashMap<&'a str, &'a Value> {
    body.pointer(&format!("/includes/{kind}"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| (str_at(x, id_key), x)).collect())
        .unwrap_or_default()
}

struct Includes<'a> {
    users: HashMap<&'a str, &'a Value>,
    tweets: HashMap<&'a str, &'a Value>,
    media: HashMap<&'a str, &'a Value>,
}

fn parse_timeline(body: &Value) -> TimelinePage {
    let inc = Includes {
        users: index(body, "users", "id"),
        tweets: index(body, "tweets", "id"),
        media: index(body, "media", "media_key"),
    };
    let tweets = body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|t| map_tweet(t, &inc)).collect())
        .unwrap_or_default();
    TimelinePage {
        tweets,
        next_cursor: next_token(body),
    }
}

fn map_user(u: &Value) -> User {
    User {
        id: str_at(u, "id").to_string(),
        name: str_at(u, "name").to_string(),
        screen_name: str_at(u, "username").to_string(),
        profile_image_url: str_at(u, "profile_image_url").to_string(),
        verified: u.get("verified").and_then(|v| v.as_bool()).unwrap_or(false),
        blue_verified: str_at(u, "verified_type") == "blue",
    }
}

fn map_media(m: &Value) -> Option<Media> {
    let kind = str_at(m, "type");
    let preview = Some(str_at(m, "preview_image_url"))
        .filter(|s| !s.is_empty())
        .map(String::from);
    let (media_type, url) = match kind {
        "photo" => (MediaType::Photo, str_at(m, "url").to_string()),
        "video" | "animated_gif" => {
            let best = m
                .get("variants")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|v| str_at(v, "content_type") == "video/mp4")
                .max_by_key(|v| u64_at(v, "bit_rate"))
                .map(|v| str_at(v, "url").to_string())
                .filter(|u| !u.is_empty());
            let media_type = if kind == "video" {
                MediaType::Video
            } else {
                MediaType::Gif
            };
            (media_type, best.or_else(|| preview.clone())?)
        }
        _ => return None,
    };
    if url.is_empty() {
        return None;
    }
    Some(Media {
        url,
        media_type,
        width: u64_at(m, "width") as u32,
        height: u64_at(m, "height") as u32,
        preview_url: preview.filter(|_| media_type != MediaType::Photo),
    })
}

/// Map one v2 tweet object; `None` when the author or id is missing from the response.
fn map_tweet(t: &Value, inc: &Includes) -> Option<Tweet> {
    let id = Some(str_at(t, "id")).filter(|s| !s.is_empty())?;
    let author = map_user(inc.users.get(str_at(t, "author_id"))?);

    let refs: HashMap<&str, &str> = t
        .get("referenced_tweets")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .map(|r| (str_at(r, "type"), str_at(r, "id")))
        .collect();
    let referenced = |kind: &str| {
        refs.get(kind)
            .and_then(|rid| inc.tweets.get(rid))
            .and_then(|r| map_tweet(r, inc))
    };

    // Retweet: show the original, remember who retweeted it. Fall back to the wrapper if it is missing.
    if let Some(mut original) = referenced("retweeted") {
        original.id = id.to_string();
        original.is_retweet = true;
        original.retweeted_by = Some(author.screen_name);
        return Some(original);
    }

    let note = t.get("note_tweet").filter(|n| n.get("text").is_some());
    let text = note
        .map_or_else(|| str_at(t, "text"), |n| str_at(n, "text"))
        .to_string();
    let entities = note
        .and_then(|n| n.get("entities"))
        .or_else(|| t.get("entities"));
    let urls = entities
        .and_then(|e| e.get("urls"))
        .and_then(|u| u.as_array())
        .into_iter()
        .flatten()
        .filter(|u| !str_at(u, "expanded_url").is_empty())
        .map(|u| UrlEntity {
            url: str_at(u, "url").to_string(),
            expanded_url: str_at(u, "expanded_url").to_string(),
            display_url: str_at(u, "display_url").to_string(),
        })
        .collect();
    let media = t
        .pointer("/attachments/media_keys")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|k| k.as_str())
        .filter_map(|k| inc.media.get(k))
        .filter_map(|m| map_media(m))
        .collect();
    let metrics = t.get("public_metrics").unwrap_or(&Value::Null);
    let quoted_tweet = referenced("quoted").map(Box::new);
    let opt = |key: &str| {
        Some(str_at(t, key))
            .filter(|s| !s.is_empty())
            .map(String::from)
    };

    Some(Tweet {
        id: id.to_string(),
        text,
        user: author,
        created_at: DateTime::parse_from_rfc3339(str_at(t, "created_at"))
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        media,
        retweet_count: u64_at(metrics, "retweet_count"),
        like_count: u64_at(metrics, "like_count"),
        reply_count: u64_at(metrics, "reply_count"),
        bookmark_count: u64_at(metrics, "bookmark_count"),
        quote_count: u64_at(metrics, "quote_count"),
        is_retweet: false,
        retweeted_by: None,
        is_quote: quoted_tweet.is_some(),
        quoted_tweet,
        conversation_id: opt("conversation_id"),
        in_reply_to_status_id: refs.get("replied_to").map(|s| s.to_string()),
        in_reply_to_screen_name: inc
            .users
            .get(str_at(t, "in_reply_to_user_id"))
            .map(|u| str_at(u, "username").to_string()),
        lang: opt("lang"),
        urls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "data": [
        {"id":"1","author_id":"u1","text":"short","created_at":"2025-01-02T03:04:05.000Z","lang":"en",
         "conversation_id":"1","public_metrics":{"retweet_count":2,"like_count":3,"reply_count":4,"quote_count":5},
         "note_tweet":{"text":"the long form text"},
         "attachments":{"media_keys":["m1","m2"]},
         "entities":{"urls":[{"url":"https://t.co/a","expanded_url":"https://example.com/a","display_url":"example.com/a"}]}},
        {"id":"2","author_id":"u2","text":"RT @u3: orig","created_at":"2025-01-02T04:00:00.000Z",
         "referenced_tweets":[{"type":"retweeted","id":"3"}]},
        {"id":"6","author_id":"u1","text":"@u2 replying","created_at":"2025-01-02T05:00:00.000Z",
         "in_reply_to_user_id":"u2","referenced_tweets":[{"type":"replied_to","id":"7"}]}
      ],
      "includes": {
        "users": [
          {"id":"u1","name":"One","username":"one","verified":false,"verified_type":"blue"},
          {"id":"u2","name":"Two","username":"two"},
          {"id":"u3","name":"Three","username":"three"},
          {"id":"u4","name":"Four","username":"four"}
        ],
        "tweets": [
          {"id":"3","author_id":"u3","text":"orig","created_at":"2025-01-01T00:00:00.000Z",
           "referenced_tweets":[{"type":"quoted","id":"4"}]},
          {"id":"4","author_id":"u4","text":"quoted body","created_at":"2024-12-31T00:00:00.000Z"}
        ],
        "media": [
          {"media_key":"m1","type":"photo","url":"https://pbs.twimg.com/media/x.jpg","width":100,"height":50},
          {"media_key":"m2","type":"video","preview_image_url":"https://pbs.twimg.com/p.jpg","variants":[
            {"content_type":"application/x-mpegURL","url":"https://v/x.m3u8"},
            {"content_type":"video/mp4","bit_rate":320000,"url":"https://v/lo.mp4"},
            {"content_type":"video/mp4","bit_rate":2176000,"url":"https://v/hi.mp4"}]}
        ]
      },
      "meta": {"next_token":"abc"}
    }"#;

    #[test]
    fn maps_v2_timeline() {
        let page = parse_timeline(&serde_json::from_str(FIXTURE).unwrap());
        assert_eq!(page.next_cursor.as_deref(), Some("abc"));
        assert_eq!(page.tweets.len(), 3);

        let a = &page.tweets[0];
        assert_eq!(a.text, "the long form text");
        assert_eq!(
            (
                a.retweet_count,
                a.like_count,
                a.reply_count,
                a.quote_count,
                a.bookmark_count
            ),
            (2, 3, 4, 5, 0)
        );
        assert!(a.user.blue_verified && !a.user.verified);
        assert_eq!(a.urls[0].expanded_url, "https://example.com/a");
        assert_eq!(a.media.len(), 2);
        assert_eq!(a.media[0].media_type, MediaType::Photo);
        assert_eq!(a.media[0].url, "https://pbs.twimg.com/media/x.jpg");
        assert_eq!(a.media[1].media_type, MediaType::Video);
        assert_eq!(a.media[1].url, "https://v/hi.mp4");
        assert_eq!(
            a.media[1].preview_url.as_deref(),
            Some("https://pbs.twimg.com/p.jpg")
        );

        let rt = &page.tweets[1];
        assert!(rt.is_retweet);
        assert_eq!(rt.retweeted_by.as_deref(), Some("two"));
        assert_eq!(rt.user.screen_name, "three");
        assert_eq!(rt.text, "orig");
        assert!(rt.is_quote);
        assert_eq!(rt.quoted_tweet.as_ref().unwrap().text, "quoted body");

        let reply = &page.tweets[2];
        assert_eq!(reply.in_reply_to_status_id.as_deref(), Some("7"));
        assert_eq!(reply.in_reply_to_screen_name.as_deref(), Some("two"));
    }
}
