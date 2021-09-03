#[macro_use]
extern crate diesel;
#[macro_use]
extern crate diesel_migrations;

use std::env;
use std::net::SocketAddr;
use std::ops::Deref;

use ::log::{debug, error, info};
use chrono::NaiveDateTime;
use chrono::Utc;
use diesel::prelude::*;
use diesel::r2d2::ConnectionManager;
use diesel::PgConnection;
use diesel_migrations::embed_migrations;
use dotenv::dotenv;
use models::Group;
use models::GroupPermission;
use models::GroupUser;
use models::PermissionStrings;
use models::User;
use models::UserPermission;
use r2d2::Pool;
use tonic::transport::Channel;
use tonic::{transport::Server, Request, Response, Status};

use userservice::user_service_server::{UserService, UserServiceServer};
use userservice::{BppUser, BppUserById, BppUserFilter, BppUserFilters, BppUsers, BppGroup};
use youtubeservice::you_tube_service_client::YouTubeServiceClient;
use youtubeservice::{GetMessageRequest, YouTubeChatMessage, YouTubeChatMessages};

use crate::log::setup_log;

mod log;
mod macros;
mod models;
mod schema;

embed_migrations!();

pub mod youtubeservice {
    tonic::include_proto!("youtubeservice");
}

pub mod userservice {
    tonic::include_proto!("userservice");
}

type Void = Result<(), Box<dyn std::error::Error>>;
type DbPool = Pool<ConnectionManager<PgConnection>>;

pub fn connect_to_database() -> Pool<ConnectionManager<PgConnection>> {
    // Get the database URL from the environment
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let manager = ConnectionManager::new(database_url);
    // Create a connection pool of 10 connections
    let pool = Pool::builder().max_size(10).build(manager).unwrap();

    // Run migrations
    let _ = embedded_migrations::run_with_output(&pool.get().unwrap(), &mut std::io::stdout());

    return pool;
}

fn calculate_hours_and_money(user: &mut User, now: &NaiveDateTime) {
    let new_hours_seconds;
    let new_hours_nanos;
    let hours_duration = chrono::Duration::seconds(user.hours_seconds)
        + chrono::Duration::nanoseconds(user.hours_nanos.into());
    let new_duration = *now - user.last_seen_at;
    let hours = hours_duration + new_duration;
    new_hours_seconds = hours.num_seconds();
    new_hours_nanos = hours.num_nanoseconds().unwrap() as i32;
    info!(
        "Updating hours of {} ({}) from {} to {}",
        user.channel_id,
        user.display_name,
        user.hours_seconds / 60 / 60,
        new_hours_seconds / 60 / 60
    );

    user.hours_seconds = new_hours_seconds;
    user.hours_nanos = new_hours_nanos;

    // Grant 1 money per minute
    // TODO: Implement payout bonus of ranks
    let new_money = user.money + new_duration.num_minutes();
    info!(
        "Updating money of {} ({}) from {} to {}",
        user.channel_id, user.display_name, user.money, new_money
    );
    user.money = new_money;
}

async fn fetch_users_from_messages(
    youtube_client: &mut YouTubeServiceClient<Channel>,
    pool: &DbPool,
) -> Void {
    let mut stream = youtube_client
        .subscribe_messages(Request::new(()))
        .await?
        .into_inner();

    while let Some(message) = stream.message().await? {
        let conn = pool.get()?;
        let now = Utc::now().naive_utc();
        let mut user = if User::check_if_exists(&message.channel_id, &conn) {
            info!("Updating existing user {}", &message.channel_id);
            // Update the user
            User::get_from_database(&message.channel_id, &conn).unwrap()
        } else {
            info!("Creating new user {}", &message.channel_id);
            // Create the user
            User::new(
                message.channel_id.clone(),
                message.display_name.clone(),
                0,
                0,
                0,
                now,
                now,
            )
        };

        user.display_name = message.display_name.clone();
        user.last_seen_at = now;

        // Determine if user was active before this message and if so, update the hours
        // if the user has been last seen less than 5 minutes ago, update the hours
        // TODO: Make the active time configurable
        if user.last_seen_at + chrono::Duration::minutes(5) < now {
            calculate_hours_and_money(&mut user, &now);
        }

        // Update the user
        user.save_to_database(&conn).unwrap();
    }

    return Ok(());
}

pub struct UserServer {
    database_pool: DbPool,
}

#[tonic::async_trait]
impl UserService for UserServer {
    async fn get_user_by_id(
        &self,
        request: tonic::Request<userservice::BppUserById>,
    ) -> Result<tonic::Response<userservice::BppUser>, tonic::Status> {
        let user_id = request.into_inner().channel_id;
        let conn = self.database_pool.get().unwrap();
        let potential_user = User::get_from_database(&user_id, &conn);

        match potential_user {
            Some(user) => {
                let bpp_user = user.to_userservice_user(&conn);
                return Ok(tonic::Response::new(bpp_user));
            },
            None => Err(tonic::Status::not_found("User not found")),
        }
    }

    async fn filter_users(
        &self,
        request: tonic::Request<userservice::BppUserFilters>,
    ) -> Result<tonic::Response<userservice::BppUsers>, tonic::Status> {
        let filter_request = request.into_inner();
        let filters = &filter_request.filters;
        let conn = self.database_pool.get().unwrap();

        use schema::bpp_users::dsl::*;
        let mut query = bpp_users.into_boxed();
        for filter in filters {
            let inner_filter = filter.filter.as_ref().unwrap();
            match inner_filter {
                userservice::bpp_user_filter::Filter::ChannelId(filter_channel_id) => {
                    query = query.filter(channel_id.eq(filter_channel_id));
                },
                userservice::bpp_user_filter::Filter::Name(filter_name) => {
                    query = query.filter(display_name.eq(filter_name));
                },
                userservice::bpp_user_filter::Filter::Hours(filter_hours) => {
                    query = query.filter(hours_seconds.eq(filter_hours));
                },
                userservice::bpp_user_filter::Filter::Money(filter_money) => {
                    query = query.filter(money.eq(filter_money));
                }
            }
        }
        
        match filter_request.sorting() {
            userservice::bpp_user_filters::SortingFields::HoursAsc => {
                query = query.order_by(hours_seconds.asc());
            },
            userservice::bpp_user_filters::SortingFields::HoursDesc => {
                query = query.order_by(hours_seconds.desc());
            },
            userservice::bpp_user_filters::SortingFields::MoneyAsc => {
                query = query.order_by(money.asc());
            },
            userservice::bpp_user_filters::SortingFields::MoneyDesc => {
                query = query.order_by(money.desc());
            },
            userservice::bpp_user_filters::SortingFields::Default => {}
        }
        let users = match query.load::<User>(&conn) {
            Ok(users) => users,
            Err(e) => {
                error!("{}", e);
                return Err(tonic::Status::internal("Failed to load users"));
            }
        };
        let users: Vec<BppUser> = users.into_iter().map(|user| user.to_userservice_user(&conn)).collect();
        let count = users.len() as i32;

        return Ok(tonic::Response::new(userservice::BppUsers { users, count }));
    }

    async fn update_user(
        &self,
        request: tonic::Request<userservice::BppUser>,
    ) -> Result<tonic::Response<userservice::BppUser>, tonic::Status> {
        todo!()
    }

    async fn update_users(
        &self,
        request: tonic::Request<userservice::BppUsers>,
    ) -> Result<tonic::Response<userservice::BppUsers>, tonic::Status> {
        todo!()
    }

    async fn delete_user(
        &self,
        request: tonic::Request<userservice::BppUser>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        todo!()
    }

    async fn delete_users(
        &self,
        request: tonic::Request<userservice::BppUsers>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        todo!()
    }

    async fn create_user(
        &self,
        request: tonic::Request<userservice::BppUser>,
    ) -> Result<tonic::Response<userservice::BppUser>, tonic::Status> {
        todo!()
    }

    async fn user_has_permission(&self, request:tonic::Request<userservice::UserPermissionCheck>) ->Result<tonic::Response<bool>,tonic::Status> {
        todo!()
    }

    async fn get_groups(
        &self,
        request: tonic::Request<()>,
    ) -> Result<tonic::Response<userservice::BppGroups>, tonic::Status> {
        todo!()
    }

    async fn update_group(
        &self,
        request: tonic::Request<userservice::BppGroup>,
    ) -> Result<tonic::Response<userservice::BppGroup>, tonic::Status> {
        todo!()
    }

    async fn update_groups(
        &self,
        request: tonic::Request<userservice::BppGroups>,
    ) -> Result<tonic::Response<userservice::BppGroups>, tonic::Status> {
        todo!()
    }

    async fn delete_group(
        &self,
        request: tonic::Request<userservice::BppGroup>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        todo!()
    }

    async fn delete_groups(
        &self,
        request: tonic::Request<userservice::BppGroups>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        todo!()
    }

    async fn create_group(
        &self,
        request: tonic::Request<userservice::CreateBppGroup>,
    ) -> Result<tonic::Response<userservice::BppGroup>, tonic::Status> {
        todo!()
    }

    async fn get_ranks(
        &self,
        request: tonic::Request<()>,
    ) -> Result<tonic::Response<userservice::BppRanks>, tonic::Status> {
        todo!()
    }

    async fn update_rank(
        &self,
        request: tonic::Request<userservice::BppRank>,
    ) -> Result<tonic::Response<userservice::BppRank>, tonic::Status> {
        todo!()
    }

    async fn update_ranks(
        &self,
        request: tonic::Request<userservice::BppRanks>,
    ) -> Result<tonic::Response<userservice::BppRanks>, tonic::Status> {
        todo!()
    }

    async fn delete_rank(
        &self,
        request: tonic::Request<userservice::BppRank>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        todo!()
    }

    async fn delete_ranks(
        &self,
        request: tonic::Request<userservice::BppRanks>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        todo!()
    }

    async fn create_rank(
        &self,
        request: tonic::Request<userservice::CreateBppRank>,
    ) -> Result<tonic::Response<userservice::BppRank>, tonic::Status> {
        todo!()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();

    setup_log(env::var_os("DEBUG").is_some());
    debug!("Debug mode activated!");

    let pool = connect_to_database();

    let youtube_address = env::var("YTS_GRPC_ADDRESS").expect("YTS_GRPC_ADDRESS must be set");
    let userservice_address = env::var("US_GRPC_ADDRESS");
    let userservice_address: SocketAddr = if userservice_address.is_err() {
        "0.0.0.0:50051".parse()?
    } else {
        userservice_address.unwrap().parse()?
    };

    let mut youtube_client = YouTubeServiceClient::connect(youtube_address).await?;
    info!("Connected to youtubeservice! Time to go on a hunt!");

    let service = UserServer {
        database_pool: pool.clone(),
    };

    info!("Starting message fetching and userservice");
    let (_, _) = tokio::join!(
        tonic::transport::Server::builder()
            .add_service(UserServiceServer::new(service))
            .serve(userservice_address),
        fetch_users_from_messages(&mut youtube_client, &pool)
    );

    return Ok(());
}