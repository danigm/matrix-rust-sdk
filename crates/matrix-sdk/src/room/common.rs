use std::{ops::Deref, sync::Arc};

use matrix_sdk_base::deserialized_responses::{MembersResponse, RoomEvent};
use matrix_sdk_common::locks::Mutex;
use ruma::{
    api::client::r0::{
        filter::RoomEventFilter,
        membership::{get_member_events, join_room_by_id, leave_room},
        message::get_message_events::{self, Direction},
        room::get_room_event,
        tag::{create_tag, delete_tag},
    },
    assign,
    events::{
        room::history_visibility::HistoryVisibility,
        tag::{TagInfo, TagName},
        AnyStateEvent, AnySyncStateEvent, EventType,
    },
    serde::Raw,
    uint, EventId, RoomId, UInt, UserId,
};

use crate::{
    media::{MediaFormat, MediaRequest, MediaType},
    room::RoomType,
    BaseRoom, Client, HttpError, HttpResult, Result, RoomMember,
};

/// A struct containing methods that are common for Joined, Invited and Left
/// Rooms
#[derive(Debug, Clone)]
pub struct Common {
    inner: BaseRoom,
    pub(crate) client: Client,
}

impl Deref for Common {
    type Target = BaseRoom;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// The result of a `Room::messages` call.
///
/// In short, this is a possibly decrypted version of the response of a
/// `room/messages` api call.
#[derive(Debug)]
pub struct Messages {
    /// The token the pagination starts from.
    pub start: String,

    /// The token the pagination ends at.
    pub end: Option<String>,

    /// A list of room events.
    pub chunk: Vec<RoomEvent>,

    /// A list of state events relevant to showing the `chunk`.
    pub state: Vec<Raw<AnyStateEvent>>,
}

impl Common {
    /// Create a new `room::Common`
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub fn new(client: Client, room: BaseRoom) -> Self {
        // TODO: Make this private
        Self { inner: room, client }
    }

    /// Leave this room.
    ///
    /// Only invited and joined rooms can be left
    pub(crate) async fn leave(&self) -> Result<()> {
        let request = leave_room::Request::new(self.inner.room_id());
        let _response = self.client.send(request, None).await?;

        Ok(())
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method
    pub(crate) async fn join(&self) -> Result<()> {
        let request = join_room_by_id::Request::new(self.inner.room_id());
        let _response = self.client.send(request, None).await?;

        Ok(())
    }

    /// Gets the avatar of this room, if set.
    ///
    /// Returns the avatar.
    /// If a thumbnail is requested no guarantee on the size of the image is
    /// given.
    ///
    /// # Arguments
    ///
    /// * `format` - The desired format of the avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client
    ///     .get_joined_room(&room_id)
    ///     .unwrap();
    /// if let Some(avatar) = room.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.avatar_url() {
            let request = MediaRequest { media_type: MediaType::Uri(url.clone()), format };
            Ok(Some(self.client.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Sends a request to `/_matrix/client/r0/rooms/{room_id}/messages` and
    /// returns a `Messages` struct that contains a chunk of room and state
    /// events (`RoomEvent` and `AnyStateEvent`).
    ///
    /// With the encryption feature, messages are decrypted if possible. If
    /// decryption fails for an individual message, that message is returned
    /// undecrypted.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{room::MessagesOptions, Client};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::r0::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// let request = MessagesOptions::backward("t47429-4392820_219380_26003_2265");
    ///
    /// let mut client = Client::new(homeserver).await.unwrap();
    /// let room = client
    ///    .get_joined_room(room_id!("!roomid:example.com"))
    ///    .unwrap();
    /// assert!(room.messages(request).await.is_ok());
    /// # });
    /// ```
    pub async fn messages(&self, options: MessagesOptions<'_>) -> Result<Messages> {
        let request = options.into_request(self.inner.room_id());
        let http_response = self.client.send(request, None).await?;

        let mut response = Messages {
            start: http_response.start,
            end: http_response.end,
            chunk: Vec::with_capacity(http_response.chunk.len()),
            state: http_response.state,
        };

        for event in http_response.chunk {
            #[cfg(feature = "encryption")]
            let event = match event.deserialize() {
                Ok(event) => self.client.decrypt_room_event(&event).await,
                Err(_) => {
                    // "Broken" messages (i.e., those that cannot be deserialized) are
                    // returned unchanged so that the caller can handle them individually.
                    RoomEvent { event, encryption_info: None }
                }
            };

            #[cfg(not(feature = "encryption"))]
            let event = RoomEvent { event, encryption_info: None };

            response.chunk.push(event);
        }

        Ok(response)
    }

    /// Fetch the event with the given `EventId` in this room.
    pub async fn event(&self, event_id: &EventId) -> Result<RoomEvent> {
        let request = get_room_event::Request::new(self.room_id(), event_id);
        let event = self.client.send(request, None).await?.event.deserialize()?;

        #[cfg(feature = "encryption")]
        return Ok(self.client.decrypt_room_event(&event).await);

        #[cfg(not(feature = "encryption"))]
        return Ok(RoomEvent { event: Raw::new(&event)?, encryption_info: None });
    }

    pub(crate) async fn request_members(&self) -> Result<Option<MembersResponse>> {
        if let Some(mutex) =
            self.client.inner.members_request_locks.get(self.inner.room_id()).map(|m| m.clone())
        {
            mutex.lock().await;

            Ok(None)
        } else {
            let mutex = Arc::new(Mutex::new(()));
            self.client
                .inner
                .members_request_locks
                .insert(self.inner.room_id().to_owned(), mutex.clone());

            let _guard = mutex.lock().await;

            let request = get_member_events::Request::new(self.inner.room_id());
            let response = self.client.send(request, None).await?;

            let response =
                self.client.base_client().receive_members(self.inner.room_id(), &response).await?;

            self.client.inner.members_request_locks.remove(self.inner.room_id());

            Ok(Some(response))
        }
    }

    async fn ensure_members(&self) -> Result<()> {
        if !self.are_events_visible() {
            return Ok(());
        }

        if !self.are_members_synced() {
            self.request_members().await?;
        }

        Ok(())
    }

    fn are_events_visible(&self) -> bool {
        if let RoomType::Invited = self.inner.room_type() {
            return matches!(
                self.inner.history_visibility(),
                HistoryVisibility::WorldReadable | HistoryVisibility::Invited
            );
        }

        true
    }

    /// Sync the member list with the server.
    ///
    /// This method will de-duplicate requests if it is called multiple times in
    /// quick succession, in that case the return value will be `None`.
    pub async fn sync_members(&self) -> Result<Option<MembersResponse>> {
        self.request_members().await
    }

    /// Get active members for this room, includes invited, joined members.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that, it might panic if it isn't run on a tokio thread.
    ///
    /// Use [active_members_no_sync()](#method.active_members_no_sync) if you
    /// want a method that doesn't do any requests.
    pub async fn active_members(&self) -> Result<Vec<RoomMember>> {
        self.ensure_members().await?;
        self.active_members_no_sync().await
    }

    /// Get active members for this room, includes invited, joined members.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing from the list.
    ///
    /// Use [active_members()](#method.active_members) if you want to ensure to
    /// always get the full member list.
    pub async fn active_members_no_sync(&self) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .active_members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Get all the joined members of this room.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [joined_members_no_sync()](#method.joined_members_no_sync) if you
    /// want a method that doesn't do any requests.
    pub async fn joined_members(&self) -> Result<Vec<RoomMember>> {
        self.ensure_members().await?;
        self.joined_members_no_sync().await
    }

    /// Get all the joined members of this room.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing from the list.
    ///
    /// Use [joined_members()](#method.joined_members) if you want to ensure to
    /// always get the full member list.
    pub async fn joined_members_no_sync(&self) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .joined_members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Get a specific member of this room.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [get_member_no_sync()](#method.get_member_no_sync) if you want a
    /// method that doesn't do any requests.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that should be fetched out of the
    /// store.
    pub async fn get_member(&self, user_id: &UserId) -> Result<Option<RoomMember>> {
        self.ensure_members().await?;
        self.get_member_no_sync(user_id).await
    }

    /// Get a specific member of this room.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [get_member()](#method.get_member) if you want to ensure to always
    /// have the full member list to chose from.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that should be fetched out of the
    /// store.
    pub async fn get_member_no_sync(&self, user_id: &UserId) -> Result<Option<RoomMember>> {
        Ok(self
            .inner
            .get_member(user_id)
            .await?
            .map(|member| RoomMember::new(self.client.clone(), member)))
    }

    /// Get all members for this room, includes invited, joined and left
    /// members.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [members_no_sync()](#method.members_no_sync) if you want a
    /// method that doesn't do any requests.
    pub async fn members(&self) -> Result<Vec<RoomMember>> {
        self.ensure_members().await?;
        self.members_no_sync().await
    }

    /// Get all members for this room, includes invited, joined and left
    /// members.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [members()](#method.members) if you want to ensure to always get
    /// the full member list.
    pub async fn members_no_sync(&self) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Get all state events of a given type in this room.
    pub async fn get_state_events(
        &self,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        self.client.store().get_state_events(self.room_id(), event_type).await.map_err(Into::into)
    }

    /// Get a specific state event in this room.
    pub async fn get_state_event(
        &self,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.client
            .store()
            .get_state_event(self.room_id(), event_type, state_key)
            .await
            .map_err(Into::into)
    }

    /// Check if all members of this room are verified and all their devices are
    /// verified.
    ///
    /// Returns true if all devices in the room are verified, otherwise false.
    #[cfg(feature = "encryption")]
    pub async fn contains_only_verified_devices(&self) -> Result<bool> {
        let user_ids = self.client.store().get_user_ids(self.room_id()).await?;

        for user_id in user_ids {
            let devices = self.client.get_user_devices(&user_id).await?;
            let any_unverified = devices.devices().any(|d| !d.verified());

            if any_unverified {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Adds a tag to the room, or updates it if it already exists.
    ///
    /// Returns the [`create_tag::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to add or update.
    ///
    /// * `tag_info` - Information about the tag, generally containing the
    ///   `order` parameter.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use ruma::events::tag::{TagInfo, TagName, UserTagName};
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// use matrix_sdk::ruma::events::tag::TagInfo;
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     let mut tag_info = TagInfo::new();
    ///     tag_info.order = Some(0.9);
    ///     let user_tag = UserTagName::from_str("u.work")?;
    ///
    ///     room.set_tag(TagName::User(user_tag), tag_info ).await?;
    /// }
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    pub async fn set_tag(
        &self,
        tag: TagName,
        tag_info: TagInfo,
    ) -> HttpResult<create_tag::Response> {
        let user_id = self.client.user_id().await.ok_or(HttpError::AuthenticationRequired)?;
        let request =
            create_tag::Request::new(&user_id, self.inner.room_id(), tag.as_ref(), tag_info);
        self.client.send(request, None).await
    }

    /// Removes a tag from the room.
    ///
    /// Returns the [`delete_tag::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to remove.
    pub async fn remove_tag(&self, tag: TagName) -> HttpResult<delete_tag::Response> {
        let user_id = self.client.user_id().await.ok_or(HttpError::AuthenticationRequired)?;
        let request = delete_tag::Request::new(&user_id, self.inner.room_id(), tag.as_ref());
        self.client.send(request, None).await
    }
}

/// Options for [`messages`][Common::messages].
///
/// See that method for details.
#[derive(Debug)]
#[non_exhaustive]
pub struct MessagesOptions<'a> {
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room from the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    pub from: &'a str,

    /// The token to stop returning events at.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room by the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    pub to: Option<&'a str>,

    /// The direction to return events in.
    pub dir: Direction,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: UInt,

    /// A [`RoomEventFilter`] to filter returned events with.
    pub filter: Option<RoomEventFilter<'a>>,
}

impl<'a> MessagesOptions<'a> {
    /// Creates `MessagesOptions` with the given start token and direction.
    ///
    /// All other parameters will be defaulted.
    pub fn new(from: &'a str, dir: Direction) -> Self {
        Self { from, to: None, dir, limit: uint!(10), filter: None }
    }

    /// Creates `MessagesOptions` with the given start token, and `dir` set to
    /// `Backward`.
    pub fn backward(from: &'a str) -> Self {
        Self::new(from, Direction::Backward)
    }

    /// Creates `MessagesOptions` with the given start token, and `dir` set to
    /// `Forward`.
    pub fn forward(from: &'a str) -> Self {
        Self::new(from, Direction::Forward)
    }

    fn into_request(self, room_id: &'a RoomId) -> get_message_events::Request {
        assign!(get_message_events::Request::new(room_id, self.from, self.dir), {
            to: self.to,
            limit: self.limit,
            filter: self.filter,
        })
    }
}
