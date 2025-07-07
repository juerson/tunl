use uuid::Uuid;

pub struct Config {
    pub uuid: Uuid,
    pub host: String,
    pub proxy_ip: Vec<String>,
    pub redirect_url: String,
    pub display_link: bool,
}
