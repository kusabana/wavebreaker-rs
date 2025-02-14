use axum::{extract::State, http::StatusCode, Form};
use axum_serde::Xml;
use diesel::{associations::HasTable, prelude::*};
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::try_join;
use tracing::{error, info, instrument};

use super::helpers::ticket_auth;
use crate::{
    models::{
        extra_song_info::ExtraSongInfo,
        players::Player,
        rivalries::Rivalry,
        scores::{NewScore, Score, ScoreWithPlayer},
        songs::{NewSong, Song},
    },
    util::{
        errors::{IntoRouteError, RouteError},
        game_types::{split_x_separated, Character, Leaderboard, League},
    },
    AppState,
};

#[derive(Deserialize)]
pub struct SongIdRequest {
    artist: String,
    song: String,
    league: League,
    //Wavebreaker-specific
    ticket: String,
    mbid: Option<String>,
    #[serde(rename = "releasembid")]
    release_mbid: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "RESULT")]
pub struct SongIdResponse {
    #[serde(rename = "@status")]
    status: String,
    #[serde(rename = "songid")]
    song_id: i32,
}

/// Attempts to get a song ID from the server.
/// If the song isn't registered on the server yet, it will be created.
///
/// # Errors
///
/// This fails if:
/// - The response fails to serialize
/// - The song fails to be created/retrieved
#[instrument(skip_all)]
pub async fn fetch_song_id(
    State(state): State<AppState>,
    Form(payload): Form<SongIdRequest>,
) -> Result<Xml<SongIdResponse>, RouteError> {
    use crate::{
        schema::{extra_song_info::dsl::*, songs::dsl::*},
        util::modifiers::{parse_from_title, remove_from_title},
    };

    let steam_player = ticket_auth(&payload.ticket, &state.steam_api).await?;

    let mut conn = state.db.get().await?;
    let parsed_modifiers = parse_from_title(&payload.song);

    // if recording MBID is provided, look it up using that + modifiers from the title
    // else, look up the song by title and artist
    if let Some(recording_mbid) = &payload.mbid {
        let song = songs
            .inner_join(extra_song_info)
            .filter(
                mbid.eq(recording_mbid)
                    .and(modifiers.is_not_distinct_from(&parsed_modifiers)),
            )
            .first::<(Song, ExtraSongInfo)>(&mut conn)
            .await
            .optional()?;

        // if song with MBID and modifiers exists, return its ID
        // else create a new song, attach the MBID to it and get metadata from MusicBrainz
        if let Some((song, _)) = song {
            info!(
                "Song {} - {} looked up by {} (Steam), league {:?}, MBID {:?}, release MBID {:?} (successful existing MBID lookup)",
                song.artist, song.title, steam_player, payload.league, payload.mbid, payload.release_mbid
            );

            Ok(Xml(SongIdResponse {
                status: "allgood".to_owned(),
                song_id: song.id,
            }))
        } else {
            info!(
                "Song {} - {} looked up by {} (Steam), league {:?}, MBID {:?}, release MBID {:?} (new MBID lookup)",
                payload.artist, payload.song, steam_player, payload.league, payload.mbid, payload.release_mbid
            );

            let song = NewSong::new(
                &remove_from_title(&payload.song),
                &payload.artist,
                parsed_modifiers,
            )
            .find_or_create(&mut conn)
            .await?;

            song.add_metadata_mbid(recording_mbid, payload.release_mbid.as_deref(), &mut conn)
                .await?;

            Ok(Xml(SongIdResponse {
                status: "allgood".to_owned(),
                song_id: song.id,
            }))
        }
    } else {
        let song = NewSong::new(
            &remove_from_title(&payload.song),
            &payload.artist,
            parsed_modifiers,
        )
        .find_or_create(&mut conn)
        .await?;

        info!(
            "Song {} - {} looked up by {} (Steam), league {:?}, MBID {:?}, release MBID {:?}",
            song.artist,
            song.title,
            steam_player,
            payload.league,
            payload.mbid,
            payload.release_mbid
        );

        Ok(Xml(SongIdResponse {
            status: "allgood".to_owned(),
            song_id: song.id,
        }))
    }
}

#[derive(Deserialize)]
pub struct SendRideRequest {
    ticket: String,
    #[serde(rename = "songid")]
    song_id: i32,
    score: i32,
    vehicle: Character,
    league: League,
    feats: String,
    #[serde(rename = "songlength")]
    song_length: i32,
    #[serde(rename = "trackshape")]
    track_shape: String,
    density: i32,
    xstats: String,
    #[serde(rename = "goldthreshold")]
    gold_threshold: i32,
    iss: i32,
    isj: i32,
    //Wavebreaker-specific
    mbid: Option<String>,
    #[serde(rename = "releasembid")]
    release_mbid: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "RESULT")]
pub struct SendRideResponse {
    #[serde(rename = "@status")]
    status: String,
    #[serde(rename = "songid")]
    song_id: i32,
    #[serde(rename = "beatscore")]
    beat_score: BeatScore,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "RESULT")]
struct BeatScore {
    #[serde(rename = "@dethroned")]
    dethroned: bool,
    #[serde(rename = "@friend")]
    friend: bool,
    #[serde(rename = "rivalname")]
    rival_name: String,
    #[serde(rename = "rivalscore")]
    rival_score: i32,
    #[serde(rename = "myscore")]
    my_score: i32,
    #[serde(rename = "reignseconds")]
    reign_seconds: i64,
}

/// Accepts score submissions by the client.
///
/// # Errors
/// This fails if:
/// - The response fails to serialize
/// - Authenticating with Steam fails
/// - The score fails to be inserted
#[instrument(skip_all)]
pub async fn send_ride(
    State(state): State<AppState>,
    Form(payload): Form<SendRideRequest>,
) -> Result<Xml<SendRideResponse>, RouteError> {
    use crate::schema::{players::dsl::*, rivalries::dsl::*, scores::dsl::*, songs::dsl::songs};

    let steam_player = ticket_auth(&payload.ticket, &state.steam_api).await?;

    info!(
        "Score received on {} from {} (Steam) with score {}, using {:?}. MBID {:?}, release MBID {:?}",
        &payload.song_id, &steam_player, &payload.score, &payload.vehicle, &payload.mbid, &payload.release_mbid
    );

    let mut conn = state.db.get().await?;
    let player: Player = Player::find_by_steam_id(steam_player)
        .first::<Player>(&mut conn)
        .await?;

    let song = songs
        .find(payload.song_id)
        .first::<Song>(&mut conn)
        .await
        .http_error("Song not found", StatusCode::NOT_FOUND)?;

    // Check the song for a top score by another player
    let current_top: Option<(Score, Player)> = scores
        .inner_join(players::table())
        .filter(song_id.eq(payload.song_id))
        .filter(league.eq(payload.league))
        .filter(player_id.ne(player.id))
        .order(score.desc())
        .first::<(Score, Player)>(&mut conn)
        .await
        .optional()?;

    // construct part of the response that's for dethroning
    let beat_score = if let Some(current_top) = current_top {
        // Check if the player dethroned the current top score
        if current_top.0.score < payload.score {
            info!(
                "Player {} (Steam) dethroned {} on {} with score {}",
                steam_player, current_top.1.id, current_top.0.song_id, payload.score
            );
        }

        // Calculate how long the current top score has been at the top before being mercilessly dethroned (part of the Brutus achievement condition!)
        let reign_duration = OffsetDateTime::now_utc() - current_top.0.submitted_at;

        // Check if the player has a rivalry with the top score holder (part of the Brutus achievement condition!)
        let rivalry = rivalries
            .find((player.id, current_top.1.id))
            .first::<Rivalry>(&mut conn)
            .await;
        // If rivalry exists, check if rivalry is mutual (we consider mutual rivalries to be friends)
        let mutual = if let Ok(rivalry) = rivalry {
            rivalry.is_mutual(&mut conn).await
        } else {
            false
        };

        BeatScore {
            dethroned: current_top.0.score < payload.score,
            friend: mutual,
            rival_name: current_top.1.username,
            rival_score: current_top.0.score,
            my_score: payload.score,
            reign_seconds: reign_duration.whole_seconds(),
        }
    } else {
        info!(
            "Player {} (Steam) got a new top score of {}",
            steam_player, payload.score
        );
        BeatScore {
            dethroned: false,
            friend: false,
            rival_name: "No one".to_owned(),
            rival_score: 143,
            my_score: 0,
            reign_seconds: 0,
        }
    };

    let new_score = NewScore::new(
        player.id,
        song.id,
        payload.league,
        payload.score,
        &split_x_separated::<i32>(&payload.track_shape)?,
        &payload
            .xstats
            .split(',')
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()?,
        payload.density,
        payload.vehicle,
        &payload.feats.split(", ").collect::<Vec<&str>>(),
        payload.song_length,
        payload.gold_threshold,
        payload.iss,
        payload.isj,
    )
    .create_or_update(&mut conn, &state.redis)
    .await?;

    // Add MusicBrainz metadata, if no extra metadata exists already
    // we're doing this here because we need the song length to search for the recording
    if let Err(e) = song
        .auto_add_metadata(payload.song_length * 10, &mut conn)
        .await
    {
        error!("Failed to add metadata for song {}: {}", song.id, e);
    }

    // TODO: Implement dethrone notifications
    Ok(Xml(SendRideResponse {
        status: "allgood".to_owned(),
        song_id: new_score.song_id,
        beat_score,
    }))
}

#[derive(Deserialize)]
pub struct GetRidesRequest {
    #[serde(rename = "songid")]
    song_id: i32,
    ticket: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename = "RESULTS")]
pub struct GetRidesResponse {
    #[serde(rename = "@status")]
    status: String,
    scores: Vec<ResponseScore>,
    #[serde(rename = "servertime")]
    server_time: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResponseScore {
    #[serde(rename = "@scoretype")]
    score_type: Leaderboard,
    league: Vec<LeagueRides>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LeagueRides {
    #[serde(rename = "@leagueid")]
    league_id: League,
    ride: Vec<Ride>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Ride {
    username: String,
    score: i32,
    #[serde(rename = "vehicleid")]
    vehicle_id: Character,
    #[serde(rename = "ridetime")]
    time: i64,
    feats: String,
    /// In centiseconds (milliseconds / 10)
    #[serde(rename = "songlength")]
    song_length: i32,
    #[serde(rename = "trafficcount")]
    traffic_count: i32,
}

fn create_league_rides(league: League, scores: Vec<ScoreWithPlayer>) -> LeagueRides {
    let mut league_rides = LeagueRides {
        league_id: league,
        ride: vec![],
    };

    for with_player in scores {
        league_rides.ride.push(Ride {
            username: with_player.player.username,
            score: with_player.score.score,
            vehicle_id: with_player.score.vehicle,
            time: with_player.score.submitted_at.unix_timestamp(),
            feats: with_player
                .score
                .feats
                .into_iter()
                .flatten()
                .collect::<Vec<String>>()
                .join(", "),
            song_length: with_player.score.song_length,
            traffic_count: with_player.score.id,
        });
    }

    league_rides
}

/// Returns scores for a given song.
///
/// # Errors
/// This fails if:
/// - The response fails to serialize
/// - Authenticating with Steam fails
#[instrument(skip_all)]
pub async fn get_rides(
    State(state): State<AppState>,
    Form(payload): Form<GetRidesRequest>,
) -> Result<Xml<GetRidesResponse>, RouteError> {
    const ALL_LEAGUES: [League; 3] = [League::Casual, League::Pro, League::Elite];

    let steam_player = ticket_auth(&payload.ticket, &state.steam_api).await?;
    info!(
        "Player {} (Steam) requesting rides of song {}",
        steam_player, payload.song_id
    );

    let mut conn = state.db.get().await?;

    let player: Player = Player::find_by_steam_id(steam_player)
        .first::<Player>(&mut conn)
        .await?;

    let mut rival_ids: Vec<i32> = player
        .get_rivals(&mut conn)
        .await?
        .iter()
        .map(|r| r.id)
        .collect();
    rival_ids.push(player.id); // So the player can see themself in rival scores

    let mut global_rides: Vec<LeagueRides> = vec![];
    let mut rival_rides: Vec<LeagueRides> = vec![];
    let mut nearby_rides: Vec<LeagueRides> = vec![];

    for league in ALL_LEAGUES {
        // can't borrow that one connection as mutable multiple times, so we need more
        let mut conn1 = state.db.get().await?;
        let mut conn2 = state.db.get().await?;

        let global_future = Score::game_get_global(payload.song_id, league, &mut conn);
        let rival_future = Score::game_get_rivals(payload.song_id, league, &rival_ids, &mut conn1);
        let nearby_future =
            Score::game_get_nearby(payload.song_id, league, player.location_id, &mut conn2);

        let (global_scores, rival_scores, nearby_scores) =
            try_join!(global_future, rival_future, nearby_future)?;

        global_rides.push(create_league_rides(league, global_scores));
        rival_rides.push(create_league_rides(league, rival_scores));
        nearby_rides.push(create_league_rides(league, nearby_scores));
    }

    Ok(Xml(GetRidesResponse {
        status: "allgood".to_owned(),
        scores: vec![
            ResponseScore {
                score_type: Leaderboard::Global,
                league: global_rides,
            },
            ResponseScore {
                score_type: Leaderboard::Friend,
                league: rival_rides,
            },
            ResponseScore {
                score_type: Leaderboard::Nearby,
                league: nearby_rides,
            },
        ],
        server_time: 143,
    }))
}
