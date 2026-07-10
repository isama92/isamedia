//! Radarr v3 REST API client. Pure HTTP, no UI dependencies. The transport
//! (auth, timeouts, status mapping, queue pagination, grabbing) is shared
//! with Sonarr in `crate::arr`; only the Radarr-specific endpoints live here.

pub mod display;
pub mod models;

use reqwest::Method;

pub use crate::arr::Error;
pub use models::{HistoryRecord, Movie, QualityProfile, QueueItem, Release, RootFolder};

#[derive(Clone, Debug)] // derived Debug is safe: Transport redacts the key
pub struct Client {
    transport: crate::arr::Transport,
}

impl Client {
    /// Build a client and validate the key with a status call; a 401/403
    /// comes back as `Error::Unauthorized` so the UI can re-prompt for it.
    pub async fn connect(host: &str, api_key: String) -> Result<Self, Error> {
        let transport = crate::arr::Transport::connect(host, api_key).await?;
        Ok(Self { transport })
    }

    pub fn host(&self) -> &str {
        &self.transport.host
    }

    /// Every movie in the library; files are embedded when present, so the
    /// detail view needs no second fetch.
    pub async fn get_movies(&self) -> Result<Vec<Movie>, Error> {
        self.transport
            .request(Method::GET, "/api/v3/movie", &[], None)
            .await
    }

    /// The whole download queue; records carry movieId directly, so no
    /// extra params are needed. Callers filter client-side.
    pub async fn get_queue(&self) -> Result<Vec<QueueItem>, Error> {
        self.transport.get_queue(&[]).await
    }

    /// Look up movies to add, by free text or by `tmdb:<id>` / `imdb:<id>`;
    /// the server parses the prefix itself, so id searches need no special
    /// handling. Returned as raw JSON: POST /movie requires fields the typed
    /// `Movie` doesn't carry (titleSlug, images), so the add forwards the
    /// whole lookup object back rather than a hand-built subset.
    pub async fn lookup_movies(&self, term: &str) -> Result<Vec<serde_json::Value>, Error> {
        self.transport
            .request(Method::GET, "/api/v3/movie/lookup", &[("term", term)], None)
            .await
    }

    /// Add a movie from a lookup object augmented with the user's choices,
    /// and return the created movie (with its new id) so the UI can open its
    /// detail straight away.
    pub async fn add_movie(&self, body: &serde_json::Value) -> Result<Movie, Error> {
        self.transport
            .send_json(Method::POST, "/api/v3/movie", &[], Some(body), None)
            .await
    }

    pub async fn get_root_folders(&self) -> Result<Vec<RootFolder>, Error> {
        self.transport.get_root_folders().await
    }

    pub async fn get_quality_profiles(&self) -> Result<Vec<QualityProfile>, Error> {
        self.transport.get_quality_profiles().await
    }

    pub async fn search_releases(&self, movie_id: i64) -> Result<Vec<Release>, Error> {
        let movie_id = movie_id.to_string();
        self.transport
            .request(
                Method::GET,
                "/api/v3/release",
                &[("movieId", movie_id.as_str())],
                Some(crate::arr::RELEASE_SEARCH_TIMEOUT),
            )
            .await
    }

    pub async fn grab_release(&self, guid: &str, indexer_id: i64) -> Result<(), Error> {
        self.transport.grab_release(guid, indexer_id).await
    }

    /// Kick off Radarr's own automatic search for one movie.
    pub async fn movie_search(&self, movie_id: i64) -> Result<(), Error> {
        self.transport
            .send(
                Method::POST,
                "/api/v3/command",
                &[],
                Some(&serde_json::json!({ "name": "MoviesSearch", "movieIds": [movie_id] })),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn delete_movie_file(&self, file_id: i64) -> Result<(), Error> {
        self.transport
            .send(
                Method::DELETE,
                &format!("/api/v3/moviefile/{file_id}"),
                &[],
                None,
                None,
            )
            .await?;
        Ok(())
    }

    /// Past grab/import/delete events of one movie, for the "grabbed before"
    /// marker in interactive search results. Unlike Sonarr's paginated
    /// /history, Radarr's /history/movie returns a plain array.
    pub async fn get_history(&self, movie_id: i64) -> Result<Vec<HistoryRecord>, Error> {
        let movie_id = movie_id.to_string();
        self.transport
            .request(
                Method::GET,
                "/api/v3/history/movie",
                &[("movieId", movie_id.as_str())],
                None,
            )
            .await
    }
}
