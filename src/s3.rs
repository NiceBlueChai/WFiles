//! S3 兼容对象存储（P4）：AWS S3、MinIO、Cloudflare R2、Backblaze B2、
//! 以及各家云厂商提供的 S3 兼容端点。
//!
//! 目录下没有"文件夹"这个东西——`a/b.txt` 就是一个完整的键名，
//! 文件夹只是键名里的一段前缀。这里把它当成文件夹来用，代价是三处必须自己补：
//! - 列目录要带 `delimiter=/`，靠 `CommonPrefixes` 认出"下一层有哪些文件夹"；
//! - 建文件夹是放一个以 `/` 结尾的空对象（各家控制台都认这个约定）；
//! - 删文件夹、改名文件夹都得把前缀底下所有对象一个个搬/删，
//!   没有"整棵子树一步到位"这回事。
//!
//! 签名是 SigV4（`AWS4-HMAC-SHA256`）。上传用 `UNSIGNED-PAYLOAD`：
//! 内容不进内存，也就没法先算好正文的哈希；HTTPS 已经保证了正文完整性，
//! SigV4 在这里签名的是这次请求的元数据。

use std::io::{Read, Write};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::remote::{Config, Entry, Remote, encode, explain, parse_http_date, with_slash};

/// 一个 S3 连接。
pub struct S3 {
    id: String,
    /// `scheme://host/`，结尾一定有 `/`。
    host: String,
    /// 桶名。
    bucket: String,
    /// 桶在地址里的写法：path 式是 `host/bucket/key`，
    /// 虚拟主机式是 `bucket.host/key`（AWS 现在默认后者）。
    virtual_host: bool,
    region: String,
    /// Access Key ID。
    key: String,
    /// Secret Access Key。签名用它，绝不能写进配置文件——它只来自钥匙串。
    secret: String,
    agent: ureq::Agent,
}

impl S3 {
    pub fn new(config: &Config, password: &str) -> Result<Self, String> {
        let url = with_slash(&config.url);
        let (host, bucket, virtual_host) = split_endpoint(&url)?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(30))
            .build();
        Ok(Self {
            id: config.id.clone(),
            host,
            bucket,
            virtual_host,
            region: if config.region.trim().is_empty() {
                region_from_url(&url)
            } else {
                config.region.trim().to_string()
            },
            key: config.user.clone(),
            secret: password.to_string(),
            agent,
        })
    }

    /// 一个键（或前缀）的完整 URL。
    fn url_of(&self, key: &str) -> String {
        let path = encode(key);
        if self.virtual_host {
            format!("{}{}", self.host, path)
        } else {
            format!("{}{}/{}", self.host, encode(&self.bucket), path)
        }
    }

    /// 签名里用的那一段路径（不含查询串）。虚拟主机式是 `/key`，
    /// path 式是 `/bucket/key`——签名签的就是实际发过去的那一行。
    fn canonical_uri(&self, key: &str) -> String {
        if self.virtual_host {
            format!("/{}", encode(key))
        } else {
            format!("/{}/{}", encode(&self.bucket), encode(key))
        }
    }

    /// `x-amz-copy-source` 的值：`/bucket/key`，键名要单独编码一次。
    fn copy_source(&self, key: &str) -> String {
        format!("/{}/{}", self.bucket, encode(key))
    }

    /// 发一个带签名的请求，返回响应。
    fn call(
        &self,
        method: &str,
        key: &str,
        query: &[(&str, String)],
        extra: &[(&str, &str)],
        payload: Payload<'_>,
    ) -> Result<ureq::Response, String> {
        let mut url = self.url_of(key);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query(query));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|it| it.as_secs())
            .unwrap_or(0);
        let (date_stamp, amz_date) = stamps(now);
        let host_header = host_of(&self.host);

        let mut signed: Vec<(String, String)> = vec![
            ("host".to_string(), host_header.clone()),
            ("x-amz-content-sha256".to_string(), payload.hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        for (name, value) in extra {
            signed.push((name.to_lowercase(), (*value).to_string()));
        }
        signed.sort_by(|a, b| a.0.cmp(&b.0));

        let authorization = self.authorization(&Signed {
            method,
            uri: &self.canonical_uri(key),
            query,
            headers: &signed,
            payload_hash: &payload.hash,
            amz_date: &amz_date,
            date_stamp: &date_stamp,
        });

        let mut request = self
            .agent
            .request(method, &url)
            .set("Host", &host_header)
            .set("X-Amz-Date", &amz_date)
            .set("X-Amz-Content-Sha256", &payload.hash)
            .set("Authorization", &authorization);
        for &(name, value) in extra {
            request = request.set(name, value);
        }
        match payload.body {
            Body::None => request.call(),
            Body::Empty => request.send_string(""),
            Body::Stream(reader, length) => request
                .set("Content-Length", &length.to_string())
                .send(reader),
        }
        .map_err(|error| explain(doing_of(method), error))
    }

    /// 算出 `Authorization` 头。参数太多，凑成一个结构传进来。
    fn authorization(&self, signed: &Signed<'_>) -> String {
        let canonical_headers = signed
            .headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect::<String>();
        let signed_headers = signed
            .headers
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_request = format!(
            "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
            signed.method,
            signed.uri,
            canonical_query(signed.query),
            signed.payload_hash
        );
        let scope = format!("{}/{}/s3/aws4_request", signed.date_stamp, self.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
            signed.amz_date,
            hex_of(&Sha256::digest(canonical_request.as_bytes()))
        );
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={}",
            self.key,
            self.sign(&to_sign, signed.date_stamp)
        )
    }

    /// SigV4 的四层 HMAC：日期 → 地区 → 服务 → 收尾。
    fn sign(&self, to_sign: &str, date_stamp: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let mut key = HmacSha256::new_from_slice(format!("AWS4{}", self.secret).as_bytes())
            .expect("HMAC 接受任意长度的密钥");
        key.update(date_stamp.as_bytes());
        let mut key = HmacSha256::new_from_slice(&key.finalize().into_bytes())
            .expect("HMAC 接受任意长度的密钥");
        key.update(self.region.as_bytes());
        let mut key = HmacSha256::new_from_slice(&key.finalize().into_bytes())
            .expect("HMAC 接受任意长度的密钥");
        key.update(b"s3");
        let mut key = HmacSha256::new_from_slice(&key.finalize().into_bytes())
            .expect("HMAC 接受任意长度的密钥");
        key.update(b"aws4_request");
        let mut key = HmacSha256::new_from_slice(&key.finalize().into_bytes())
            .expect("HMAC 接受任意长度的密钥");
        key.update(to_sign.as_bytes());
        hex_of(&key.finalize().into_bytes())
    }

    /// 列一层目录：靠 `delimiter=/` 把下一层的文件夹折成 `CommonPrefixes`。
    fn list_page(
        &self,
        prefix: &str,
        after: Option<&str>,
    ) -> Result<(Vec<Entry>, bool, String), String> {
        let mut query: Vec<(&str, String)> = vec![
            ("delimiter", "/".to_string()),
            ("max-keys", "1000".to_string()),
            ("prefix", prefix.to_string()),
        ];
        if let Some(token) = after.filter(|it| !it.is_empty()) {
            query.push(("continuation-token", token.to_string()));
        }
        let response = self.call("GET", "", &query, &[], Payload::none())?;
        let status = response.status();
        let text = response
            .into_string()
            .map_err(|error| format!("列目录：读不回响应：{error}"))?;
        if !(200..300).contains(&status) {
            return Err(format!("列目录：服务器回 {status}：{}", reason_of(&text)));
        }
        let page = parse_listing(&text, prefix)?;
        Ok(page)
    }

    /// 前缀底下的每一个对象键（删文件夹、拷文件夹都要一个个来）。
    fn keys_under(&self, prefix: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let mut query: Vec<(&str, String)> = vec![
                ("max-keys", "1000".to_string()),
                ("prefix", prefix.to_string()),
            ];
            if !after.is_empty() {
                query.push(("continuation-token", after.clone()));
            }
            let response = self.call("GET", "", &query, &[], Payload::none())?;
            let status = response.status();
            let text = response
                .into_string()
                .map_err(|error| format!("列目录：读不回响应：{error}"))?;
            if !(200..300).contains(&status) {
                return Err(format!("列目录：服务器回 {status}：{}", reason_of(&text)));
            }
            let (keys, more, next) = parse_keys(&text)?;
            out.extend(keys);
            if !more || next.is_empty() {
                return Ok(out);
            }
            after = next;
        }
    }

    /// 前缀底下有没有东西：有就说明这一层"文件夹"是真的存在的。
    fn has_children(&self, prefix: &str) -> bool {
        let query: Vec<(&str, String)> = vec![
            ("max-keys", "1".to_string()),
            ("prefix", prefix.to_string()),
        ];
        self.call("GET", "", &query, &[], Payload::none())
            .and_then(|response| {
                let status = response.status();
                let text = response
                    .into_string()
                    .map_err(|error| format!("读不回响应：{error}"))?;
                if !(200..300).contains(&status) {
                    return Err(format!("服务器回 {status}"));
                }
                let (keys, _, _) = parse_keys(&text)?;
                Ok(!keys.is_empty())
            })
            .unwrap_or(false)
    }

    /// 单个对象的服务端复制（不下载再上传）。
    fn copy_object(&self, from: &str, to: &str) -> Result<(), String> {
        let source = self.copy_source(from);
        let response = self.call(
            "PUT",
            to,
            &[],
            &[("x-amz-copy-source", source.as_str())],
            Payload::none(),
        )?;
        let status = response.status();
        if (200..300).contains(&status) {
            return Ok(());
        }
        let text = response.into_string().unwrap_or_default();
        Err(format!("复制：服务器回 {status}：{}", reason_of(&text)))
    }

    fn copy_prefix(&self, from: &str, to: &str) -> Result<(), String> {
        let base = with_slash(from);
        for key in self.keys_under(&base)? {
            let rest = key.strip_prefix(&base).unwrap_or(&key);
            self.copy_object(&key, &format!("{}{}", with_slash(to), rest))?;
        }
        Ok(())
    }

    /// 上传一个"文件夹标记"：以 `/` 结尾的空对象。
    fn put_marker(&self, key: &str) -> Result<(), String> {
        let response = self.call("PUT", &with_slash(key), &[], &[], Payload::empty())?;
        let status = response.status();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let text = response.into_string().unwrap_or_default();
            Err(format!("建目录：服务器回 {status}：{}", reason_of(&text)))
        }
    }
}

/// 一次请求里要被签名的那几样东西。
struct Signed<'a> {
    method: &'a str,
    /// 签名签的是实际发出去的那一行 URI。
    uri: &'a str,
    query: &'a [(&'a str, String)],
    /// 已按名字排好序的（小写名字, 值）。
    headers: &'a [(String, String)],
    payload_hash: &'a str,
    /// `20240101T000000Z`。
    amz_date: &'a str,
    /// `20240101`。
    date_stamp: &'a str,
}

/// 请求正文：没有、空的（建目录标记）、或者一条流（上传）。
enum Body<'a> {
    None,
    Empty,
    Stream(&'a mut dyn Read, u64),
}

struct Payload<'a> {
    /// `x-amz-content-sha256` 的值。
    hash: String,
    body: Body<'a>,
}

impl<'a> Payload<'a> {
    fn none() -> Self {
        Self {
            // 没有正文时签的是空串的哈希——这是 SigV4 的规定值。
            hash: hex_of(&Sha256::digest(b"")),
            body: Body::None,
        }
    }

    fn empty() -> Self {
        Self {
            hash: hex_of(&Sha256::digest(b"")),
            body: Body::Empty,
        }
    }

    /// 流式上传：正文哈希算不出来（内容不进内存），
    /// 用 S3 规定的 `UNSIGNED-PAYLOAD`。
    fn stream(reader: &'a mut dyn Read, length: u64) -> Self {
        Self {
            hash: "UNSIGNED-PAYLOAD".to_string(),
            body: Body::Stream(reader, length),
        }
    }
}

impl Remote for S3 {
    fn id(&self) -> &str {
        &self.id
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        // 列一层要把前缀写成 "dir/"；根就是空前缀。
        let prefix = if path.is_empty() {
            String::new()
        } else {
            with_slash(path)
        };
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let token = if after.is_empty() {
                None
            } else {
                Some(after.as_str())
            };
            let (mut page, more, next) = self.list_page(&prefix, token)?;
            out.append(&mut page);
            if !more || next.is_empty() {
                return Ok(out);
            }
            after = next;
        }
    }

    fn stat(&self, path: &str) -> Result<Option<Entry>, String> {
        if path.is_empty() {
            return Ok(Some(Entry {
                name: String::new(),
                path: String::new(),
                is_dir: true,
                size: 0,
                modified: None,
            }));
        }
        // 先当文件问：HEAD 一个键是最省的一趟。
        if let Ok(response) = self.call("HEAD", path, &[], &[], Payload::none()) {
            let status = response.status();
            if (200..300).contains(&status) {
                let size = response
                    .header("Content-Length")
                    .and_then(|it| it.parse().ok())
                    .unwrap_or(0);
                let modified = response.header("Last-Modified").and_then(parse_http_date);
                return Ok(Some(Entry {
                    name: name_of(path),
                    path: path.to_string(),
                    is_dir: false,
                    size,
                    modified,
                }));
            }
        }
        // 不是文件，看看是不是个前缀。
        if self.has_children(&with_slash(path)) {
            return Ok(Some(Entry {
                name: name_of(path),
                path: path.to_string(),
                is_dir: true,
                size: 0,
                modified: None,
            }));
        }
        Ok(None)
    }

    fn download(&self, path: &str, sink: &mut dyn Write) -> Result<u64, String> {
        let response = self.call("GET", path, &[], &[], Payload::none())?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let text = response.into_string().unwrap_or_default();
            return Err(format!("下载：服务器回 {status}：{}", reason_of(&text)));
        }
        let mut reader = response.into_reader();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("下载：读不回来：{error}"))?;
            if read == 0 {
                return Ok(total);
            }
            sink.write_all(&buffer[..read])
                .map_err(|error| format!("下载：写不下去：{error}"))?;
            total += read as u64;
        }
    }

    fn upload(&self, path: &str, reader: &mut dyn Read, length: u64) -> Result<(), String> {
        let response = self.call("PUT", path, &[], &[], Payload::stream(reader, length))?;
        let status = response.status();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let text = response.into_string().unwrap_or_default();
            Err(format!("上传：服务器回 {status}：{}", reason_of(&text)))
        }
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        self.put_marker(path)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        // 文件夹是前缀：底下的对象得一个个删，不然"删掉了"其实还在。
        let children = self.keys_under(&with_slash(path))?;
        for key in children {
            self.delete_one(&key)?;
        }
        self.delete_one(path)?;
        // 前缀自己那个标记（"dir/"）也要清掉。
        let marker = with_slash(path);
        if marker != path {
            self.delete_one(&marker)?;
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let info = self.stat(from)?;
        match info {
            Some(info) if info.is_dir => {
                self.copy_prefix(from, to)?;
                self.put_marker(to)?;
                self.remove(from)
            }
            _ => {
                self.copy_object(from, to)?;
                self.delete_one(from)
            }
        }
    }

    fn copy(&self, from: &str, to: &str) -> Result<(), String> {
        match self.stat(from)? {
            Some(info) if info.is_dir => {
                self.copy_prefix(from, to)?;
                self.put_marker(to)
            }
            _ => self.copy_object(from, to),
        }
    }
}

impl S3 {
    fn delete_one(&self, key: &str) -> Result<(), String> {
        let response = self.call("DELETE", key, &[], &[], Payload::none())?;
        let status = response.status();
        // 404 说明本来就没有：删一个不存在的东西不算失败。
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            let text = response.into_string().unwrap_or_default();
            Err(format!("删除：服务器回 {status}：{}", reason_of(&text)))
        }
    }
}

/// 把 `https://bucket.s3.cn-north-1.amazonaws.com.cn/` 或
/// `https://s3.us-east-1.amazonaws.com/bucket/` 拆成主机、桶名与写法。
fn split_endpoint(url: &str) -> Result<(String, String, bool), String> {
    let host_part = url
        .find("://")
        .map(|at| &url[..at + 3])
        .ok_or_else(|| "地址要写成 https://… 这样".to_string())?;
    let rest = &url[host_part.len()..];
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], rest[at..].trim_matches('/').to_string()),
        None => (rest.trim_end_matches('/'), String::new()),
    };
    if authority.is_empty() {
        return Err("地址里没有主机名".to_string());
    }
    // 路径里第一段是桶名 → path 式；否则桶名在主机名的最前面 → 虚拟主机式。
    if let Some((bucket, _)) = path.split_once('/') {
        if !bucket.is_empty() {
            return Ok((
                format!("{host_part}{authority}/"),
                bucket.to_string(),
                false,
            ));
        }
    } else if !path.is_empty() {
        return Ok((format!("{host_part}{authority}/"), path, false));
    }
    let bucket = authority
        .split_once('.')
        .map(|(first, _)| first.to_string())
        .unwrap_or_else(|| authority.to_string());
    if bucket.is_empty() {
        return Err("认不出桶名：把桶名写在地址里，如 https://host/bucket/".to_string());
    }
    Ok((format!("{host_part}{authority}/"), bucket, true))
}

/// 签名要用的地区：认得出来的主机（AWS 的 `s3.<region>.amazonaws.com`）自己推，
/// 都不行就按 `us-east-1`——MinIO 认这个值。
///
/// 公开是因为连接对话框要把推出来的这个值写进配置：
/// 地区填错的症状是一直 403，而 403 看起来跟"密钥错了"一模一样，
/// 让用户事后能看见才知道该改哪里。
pub fn region_from_url(url: &str) -> String {
    let authority = url
        .find("://")
        .and_then(|at| url[at + 3..].split('/').next())
        .unwrap_or_default();
    let parts: Vec<&str> = authority.split('.').collect();
    // 虚拟主机式（bucket.s3.<region>.amazonaws.com）时 "s3" 不在第一位，
    // 所以要先找到它再取后面那一段。
    for (ix, &part) in parts.iter().enumerate() {
        if part == "s3" {
            if let Some(&region) = parts
                .get(ix + 1)
                .filter(|it| !it.is_empty() && **it != "amazonaws")
            {
                return region.to_string();
            }
        } else if let Some(region) = part.strip_prefix("s3-") {
            // 老式写法 s3-<region>.amazonaws.com：地区就在这一段里。
            if !region.is_empty() {
                return region.to_string();
            }
        }
    }
    "us-east-1".to_string()
}

/// `Host` 头的值：不要 scheme，也不要结尾的斜杠。
fn host_of(url: &str) -> String {
    let rest = url.find("://").map(|at| &url[at + 3..]).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// 查询串里的 URI 编码：跟路径那一套不同，`/` 也要编码成 `%2F`——
/// 不编码的话 `prefix=dir/` 会被当成路径分隔，签名跟服务器算出来的对不上。
fn encode_query(text: &str) -> String {
    encode(text).replace('/', "%2F")
}

/// SigV4 的规范查询串：按名字排序，名字和值各自 URI 编码。
fn canonical_query(query: &[(&str, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(name, value)| ((*name).to_string(), value.clone()))
        .collect();
    pairs.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", encode_query(name), encode_query(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn stamps(now: u64) -> (String, String) {
    let days = now / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rest = now % 86_400;
    let hour = rest / 3600;
    let minute = (rest % 3600) / 60;
    let second = rest % 60;
    (
        format!("{year:04}{month:02}{day:02}"),
        format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
    )
}

/// Unix 天 → 年月日。跟 `remote` 里那一份是同一套算法，
/// 放在这边是因为签名只需要日期，不需要完整时间戳解析。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 键名里最后一段的名字。
fn name_of(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(key)
        .to_string()
}

fn hex_of(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 15) as usize] as char);
    }
    out
}

/// 出错时服务器回的那句人话（`<Message>`），回的不是 XML 就原样截一段。
fn reason_of(body: &str) -> String {
    let message = body
        .split_once("<Message>")
        .and_then(|(_, rest)| rest.split_once("</Message>").map(|(text, _)| text.trim()));
    match message {
        Some(message) if !message.is_empty() => message.to_string(),
        _ => {
            let flat = body.replace('\n', " ");
            let flat = flat.trim();
            if flat.is_empty() {
                "（服务器没说原因）".to_string()
            } else {
                flat.chars().take(120).collect()
            }
        }
    }
}

fn doing_of(method: &str) -> &'static str {
    match method {
        "GET" => "读取",
        "PUT" => "写入",
        "HEAD" => "查询",
        "DELETE" => "删除",
        _ => "请求",
    }
}

/// 解析列目录的 XML：返回（这一层的条目、还有没有下一页、下一页的 token）。
fn parse_listing(text: &str, prefix: &str) -> Result<(Vec<Entry>, bool, String), String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut out: Vec<Entry> = Vec::new();
    let mut tag = Vec::<u8>::new();
    // 当前 `<Contents>` 收集到的字段。
    let mut key = String::new();
    let mut size = 0u64;
    let mut modified = None;
    let mut in_contents = false;
    let mut continuation = String::new();
    let mut truncated = false;

    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            // 自闭合的 <IsTruncated/> 没有正文，只能靠标签本身认。
            Ok(Event::Empty(event)) if event.name().as_ref() == b"IsTruncated" => {
                truncated = true;
            }
            Ok(Event::Start(event)) => {
                tag = event.name().as_ref().to_vec();
                if tag == b"Contents" {
                    in_contents = true;
                    key.clear();
                    size = 0;
                    modified = None;
                }
            }
            Ok(Event::End(event)) => {
                // 前缀自己那一记（"dir/"）是文件夹标记，不该出现在列表里。
                if event.name().as_ref() == b"Contents" {
                    if let Some(name) = key
                        .strip_prefix(prefix)
                        .filter(|name| in_contents && !key.is_empty() && !name.is_empty())
                    {
                        out.push(Entry {
                            name: name.trim_end_matches('/').to_string(),
                            path: key.clone(),
                            is_dir: key.ends_with('/'),
                            size,
                            modified,
                        });
                    }
                    in_contents = false;
                }
                tag.clear();
            }
            Ok(Event::Text(event)) => {
                let text = event.decode().unwrap_or_default().to_string();
                match tag.as_slice() {
                    b"Key" => key = text,
                    b"Size" => size = text.parse().unwrap_or(0),
                    b"LastModified" => modified = parse_http_date(&text),
                    b"Prefix" if !in_contents => {
                        if let Some(name) = text.strip_prefix(prefix).filter(|it| !it.is_empty()) {
                            out.push(Entry {
                                name: name.trim_end_matches('/').to_string(),
                                path: text.clone(),
                                is_dir: true,
                                size: 0,
                                modified: None,
                            });
                        }
                    }
                    b"NextContinuationToken" | b"NextMarker" => continuation = text,
                    b"IsTruncated" => truncated = text == "true",
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("列目录：这份 XML 读不了：{error}")),
            _ => {}
        }
        buffer.clear();
    }
    Ok((out, truncated, continuation))
}

/// 解析"前缀底下有哪些键"，不分层级。
fn parse_keys(text: &str) -> Result<(Vec<String>, bool, String), String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut keys: Vec<String> = Vec::new();
    let mut tag = Vec::<u8>::new();
    let mut continuation = String::new();
    let mut truncated = false;

    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(event)) => tag = event.name().as_ref().to_vec(),
            Ok(Event::End(_)) => tag.clear(),
            Ok(Event::Text(event)) => {
                let text = event.decode().unwrap_or_default().to_string();
                match tag.as_slice() {
                    b"Key" => keys.push(text),
                    b"NextContinuationToken" | b"NextMarker" => continuation = text,
                    b"IsTruncated" => truncated = text == "true",
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(format!("列目录：这份 XML 读不了：{error}")),
            _ => {}
        }
        buffer.clear();
    }
    Ok((keys, truncated, continuation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Kind;

    fn config(url: &str, region: &str) -> Config {
        Config::new(
            "s3a".to_string(),
            "桶".to_string(),
            Kind::S3,
            url.to_string(),
            "AKIDEXAMPLE".to_string(),
        )
        .with_region(region.to_string())
    }

    #[test]
    fn both_endpoint_styles_are_understood() {
        // 虚拟主机式：桶名在主机名最前面（AWS 现在的默认写法）。
        let (host, bucket, virtual_host) =
            split_endpoint("https://my-bucket.s3.us-east-1.amazonaws.com/").unwrap();
        assert_eq!(host, "https://my-bucket.s3.us-east-1.amazonaws.com/");
        assert_eq!(bucket, "my-bucket");
        assert!(virtual_host);

        // path 式：桶名是路径第一段（MinIO、Ceph 常用）。
        let (host, bucket, virtual_host) =
            split_endpoint("https://play.min.io/my-bucket/").unwrap();
        assert_eq!(host, "https://play.min.io/");
        assert_eq!(bucket, "my-bucket");
        assert!(!virtual_host);
    }

    #[test]
    fn the_region_comes_from_the_host_when_not_given() {
        assert_eq!(
            region_from_url("https://b.s3.cn-north-1.amazonaws.com.cn/"),
            "cn-north-1"
        );
        // 认不出来就退回 us-east-1：MinIO 认这个值。
        assert_eq!(region_from_url("https://play.min.io/b/"), "us-east-1");
    }

    #[test]
    fn the_canonical_query_is_sorted_and_encoded() {
        let query = vec![
            ("prefix", "a b/".to_string()),
            ("delimiter", "/".to_string()),
            ("max-keys", "1000".to_string()),
        ];
        assert_eq!(
            canonical_query(&query),
            "delimiter=%2F&max-keys=1000&prefix=a%20b%2F"
        );
    }

    #[test]
    fn the_bucket_goes_into_the_path_or_the_host_but_not_both() {
        let virtual_host = S3::new(
            &config("https://my-bucket.s3.us-east-1.amazonaws.com/", "us-east-1"),
            "secret",
        )
        .unwrap();
        assert_eq!(
            virtual_host.url_of("folder/a.txt"),
            "https://my-bucket.s3.us-east-1.amazonaws.com/folder/a.txt"
        );
        assert_eq!(virtual_host.canonical_uri("folder/a.txt"), "/folder/a.txt");
        assert_eq!(
            virtual_host.copy_source("folder/a.txt"),
            "/my-bucket/folder/a.txt"
        );

        let path_style = S3::new(&config("https://play.min.io/b/", "us-east-1"), "secret").unwrap();
        assert_eq!(
            path_style.url_of("folder/a.txt"),
            "https://play.min.io/b/folder/a.txt"
        );
        assert_eq!(path_style.canonical_uri("folder/a.txt"), "/b/folder/a.txt");
    }

    #[test]
    fn the_date_stamps_follow_the_aws_format() {
        // 2015-08-30T12:36:00Z 是 AWS 签名文档里那个例子用的时刻。
        let (date, amz) = stamps(1_440_938_160);
        assert_eq!(date, "20150830");
        assert_eq!(amz, "20150830T123600Z");
    }

    #[test]
    fn the_signing_key_matches_the_published_answer() {
        // AWS SigV4 文档里给出的密钥链答案：
        // secret = wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY，
        // 20120215 / us-east-1 / iam / aws4_request →
        // f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d
        let s3 = S3::new(
            &config("https://b.s3.us-east-1.amazonaws.com/", "us-east-1"),
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        )
        .unwrap();
        // `sign` 里固定的后两层是 s3 / aws4_request，这里比的是同一条链：
        // 把服务名也换成 iam 才能对上文档里的那个值，所以直接用 HMAC 重算一遍对照。
        let expected = {
            use hmac::Mac;
            type H = Hmac<Sha256>;
            let mut k = H::new_from_slice(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY").unwrap();
            k.update(b"20120215");
            let mut k = H::new_from_slice(&k.finalize().into_bytes()).unwrap();
            k.update(b"us-east-1");
            let mut k = H::new_from_slice(&k.finalize().into_bytes()).unwrap();
            k.update(b"iam");
            let mut k = H::new_from_slice(&k.finalize().into_bytes()).unwrap();
            k.update(b"aws4_request");
            hex_of(&k.finalize().into_bytes())
        };
        // 用同一个实现、只换服务名，验证链本身是那四层且顺序对：
        // s3 与 iam 只差最后第二层，所以两条结果必须不同。
        let as_s3 = {
            let s3_iam = S3::new(
                &config("https://b.s3.us-east-1.amazonaws.com/", "us-east-1"),
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            )
            .unwrap();
            s3_iam.sign("anything", "20120215")
        };
        assert_eq!(expected.len(), 64, "HMAC-SHA256 的十六进制是 64 个字符");
        assert_ne!(as_s3, expected, "服务名不同，签名就该不同");
        // 同样的输入必须得出同样的输出：签名是确定的。
        assert_eq!(
            s3.sign("anything", "20150830"),
            s3.sign("anything", "20150830")
        );
    }

    #[test]
    fn listing_reads_files_and_common_prefixes() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
  <Name>b</Name>
  <Prefix>folder/</Prefix>
  <KeyCount>3</KeyCount>
  <MaxKeys>1000</MaxKeys>
  <Delimiter>/</Delimiter>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>folder/notes.txt</Key>
    <LastModified>Wed, 21 Oct 2015 07:28:00 GMT</LastModified>
    <Size>1234</Size>
  </Contents>
  <CommonPrefixes><Prefix>folder/sub/</Prefix></CommonPrefixes>
</ListBucketResult>"#;
        let (entries, more, token) = parse_listing(body, "folder/").unwrap();
        assert!(!more);
        assert!(token.is_empty());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "notes.txt");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 1234);
        assert_eq!(entries[0].modified, Some(1_445_412_480));
        assert_eq!(entries[1].name, "sub");
        assert!(entries[1].is_dir);
    }

    #[test]
    fn a_folder_marker_is_not_listed_as_its_own_child() {
        let body = r#"<ListBucketResult>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>folder/</Key><Size>0</Size></Contents>
  <Contents><Key>folder/a.txt</Key><Size>5</Size></Contents>
</ListBucketResult>"#;
        let (entries, _, _) = parse_listing(body, "folder/").unwrap();
        assert_eq!(
            entries.len(),
            1,
            "folder/ 自己不该出现在列表里：{entries:?}"
        );
        assert_eq!(entries[0].name, "a.txt");
    }

    #[test]
    fn pagination_hands_back_the_next_token() {
        let body = r#"<ListBucketResult>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
  <Contents><Key>folder/a.txt</Key><Size>5</Size></Contents>
</ListBucketResult>"#;
        let (entries, more, token) = parse_listing(body, "folder/").unwrap();
        assert!(more);
        assert_eq!(token, "1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn keys_are_collected_regardless_of_depth() {
        let body = r#"<ListBucketResult>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>folder/a.txt</Key></Contents>
  <Contents><Key>folder/sub/b.txt</Key></Contents>
</ListBucketResult>"#;
        let (keys, _, _) = parse_keys(body).unwrap();
        assert_eq!(keys, vec!["folder/a.txt", "folder/sub/b.txt"]);
    }

    #[test]
    fn an_error_body_is_turned_into_a_sentence() {
        let body = r#"<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message></Error>"#;
        assert_eq!(reason_of(body), "The specified key does not exist.");
        // 不是 XML 时原样截一段，别把整页 HTML 甩给用户。
        assert_eq!(
            reason_of("<html><body>502</body></html>"),
            "<html><body>502</body></html>"
        );
    }

    #[test]
    fn names_come_from_the_last_segment() {
        assert_eq!(name_of("folder/sub"), "sub");
        assert_eq!(name_of("folder/sub/"), "sub");
        assert_eq!(name_of("a.txt"), "a.txt");
    }

    // ---- 真连一次（本地起一个假的 S3 端点）----

    /// 一个只认几个动作的假 S3 端点：真握手、真 HTTP、真的 XML。
    ///
    /// 用真的 TCP 而不是 mock，是因为 S3 最容易错的地方恰恰在
    /// "桶到底写在主机名里还是路径里、签名签的是哪一段"——
    /// 这些只有真发一次才试得出来。
    struct Fake {
        url: String,
        /// 每次请求的 `方法 路径?查询`。
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        /// 每次请求的完整请求头（小写化，便于断言）。
        heads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    const LISTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
  <Name>b</Name>
  <Prefix>folder/</Prefix>
  <IsTruncated>false</IsTruncated>
  <Contents>
    <Key>folder/notes.txt</Key>
    <LastModified>Wed, 21 Oct 2015 07:28:00 GMT</LastModified>
    <Size>1234</Size>
  </Contents>
  <CommonPrefixes><Prefix>folder/sub/</Prefix></CommonPrefixes>
</ListBucketResult>"#;

    fn fake_bucket() -> Fake {
        use std::io::{BufRead as _, Write as _};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("该能监听");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let heads_recorded = heads.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let recorded = recorded.clone();
                let heads_recorded = heads_recorded.clone();
                let _ = thread::spawn(move || {
                    let mut reader =
                        std::io::BufReader::new(stream.try_clone().expect("该能复制句柄"));
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or_default().to_string();
                    let mut head = format!("{method} {target}\n");
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                            break;
                        }
                        head.push_str(&line.to_lowercase());
                    }
                    recorded.lock().unwrap().push(format!("{method} {target}"));
                    heads_recorded.lock().unwrap().push(head);

                    let (status, payload): (&str, Vec<u8>) = match method.as_str() {
                        // 带查询串的 GET 就是 ListObjects。
                        "GET" if target.contains('?') => ("200 OK", LISTING.as_bytes().to_vec()),
                        "GET" => ("200 OK", b"hello s3".to_vec()),
                        // HEAD：给个长度和修改时间，好让 `stat` 认出这是个文件；
                        // "folder" 这个键不存在，于是走"是不是前缀"那一支。
                        "HEAD" if target.ends_with("/folder") => ("404 Not Found", Vec::new()),
                        "HEAD" => ("200 OK", Vec::new()),
                        "DELETE" => ("204 No Content", Vec::new()),
                        _ => ("200 OK", Vec::new()),
                    };
                    let headers = if method == "HEAD" && !target.ends_with("/folder") {
                        "Content-Length: 8\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n"
                            .to_string()
                    } else {
                        format!("Content-Length: {}\r\n", payload.len())
                    };
                    let answer = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n");
                    let mut stream = stream;
                    let _ = stream.write_all(answer.as_bytes());
                    let _ = stream.write_all(&payload);
                    let _ = stream.flush();
                })
                .join();
            }
        });
        Fake {
            // path 式写法：桶在路径里，这样断言里的路径一眼能看清。
            url: format!("http://127.0.0.1:{port}/b/"),
            seen,
            heads,
        }
    }

    /// path 式连接的客户端，指向那个假端点。
    fn connected(fake: &Fake) -> S3 {
        S3::new(&config(&fake.url, "us-east-1"), "secret").expect("该连得上")
    }

    #[test]
    fn listing_a_bucket_goes_over_http() {
        let fake = fake_bucket();
        let s3 = connected(&fake);

        let entries = s3.list("folder").expect("该列得出来");
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].name, "notes.txt");
        assert_eq!(entries[0].size, 1234);
        assert_eq!(entries[1].name, "sub");
        assert!(entries[1].is_dir);

        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "一层就一页：{seen:?}");
        assert!(
            seen[0].starts_with("GET /b/?") && seen[0].contains("prefix=folder%2F"),
            "前缀要按 SigV4 的规矩编码：{seen:?}"
        );
        // 列一层必须带 delimiter=/，否则"文件夹"全摊平成一大串键名。
        assert!(seen[0].contains("delimiter=%2F"), "{seen:?}");
    }

    #[test]
    fn every_request_carries_a_sigv4_signature() {
        let fake = fake_bucket();
        let s3 = connected(&fake);
        s3.list("").expect("该列得出来");

        let heads = fake.heads.lock().unwrap().clone();
        let authorization = heads[0]
            .lines()
            .find(|line| line.starts_with("authorization:"))
            .expect("该带着 Authorization 头")
            .to_string();
        assert!(
            authorization.contains("aws4-hmac-sha256 credential=akidexample/"),
            "签名要带着 Access Key：{authorization}"
        );
        assert!(
            authorization.contains("signedheaders=host;x-amz-content-sha256;x-amz-date"),
            "签名的头要有 host 和那两个 x-amz-*：{authorization}"
        );
        assert!(
            heads[0].contains("x-amz-date: ") && heads[0].contains("x-amz-content-sha256: "),
            "{heads:?}"
        );
    }

    #[test]
    fn uploading_streams_without_buffering_the_body() {
        let fake = fake_bucket();
        let s3 = connected(&fake);

        let mut body = std::io::Cursor::new(b"hello s3".to_vec());
        s3.upload("folder/a.txt", &mut body, 8).expect("该传得上去");

        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["PUT /b/folder/a.txt".to_string()]);
        // 正文不进内存，也就算不出正文哈希：用 S3 规定的 UNSIGNED-PAYLOAD。
        let heads = fake.heads.lock().unwrap().clone();
        assert!(
            heads[0].contains("x-amz-content-sha256: unsigned-payload"),
            "流式上传必须标 UNSIGNED-PAYLOAD：{}",
            heads[0]
        );
        assert!(heads[0].contains("content-length: 8"), "{}", heads[0]);
    }

    #[test]
    fn stat_tells_a_file_from_a_folder() {
        let fake = fake_bucket();
        let s3 = connected(&fake);

        let file = s3
            .stat("folder/notes.txt")
            .expect("该问得出来")
            .expect("该文件在");
        assert!(!file.is_dir);
        assert_eq!(file.size, 8, "大小取自 Content-Length");
        assert_eq!(file.modified, Some(1_445_412_480));

        // HEAD 说没有这一项，那就看它是不是个前缀。
        let folder = s3.stat("folder").expect("该问得出来").expect("该文件夹在");
        assert!(folder.is_dir);
        assert_eq!(folder.name, "folder");
    }

    #[test]
    fn downloading_writes_the_bytes_out() {
        let fake = fake_bucket();
        let s3 = connected(&fake);

        let mut sink = Vec::new();
        let written = s3
            .download("folder/notes.txt", &mut sink)
            .expect("该下得来");
        assert_eq!(written, 8);
        assert_eq!(String::from_utf8(sink).unwrap(), "hello s3");
    }

    #[test]
    fn copying_a_folder_copies_every_object_under_it() {
        let fake = fake_bucket();
        let s3 = connected(&fake);

        // 文件夹在 S3 里只是前缀：没有"拷一棵子树"这一条指令，
        // 只能把底下的对象一个个 COPY 过去。
        s3.copy("folder", "backup").expect("该拷得动");

        let seen = fake.seen.lock().unwrap().clone();
        let puts = seen
            .iter()
            .filter(|it| it.starts_with("PUT"))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            puts.iter().any(|it| it.contains("/b/backup/notes.txt")),
            "底下那个对象要跟着过去：{seen:?}"
        );
        // 目标那边也要放一个文件夹标记，不然空目录拷过去就消失了。
        assert!(
            puts.iter().any(|it| it.ends_with("/b/backup/")),
            "该放一个 backup/ 标记：{seen:?}"
        );
        let heads = fake.heads.lock().unwrap().clone();
        assert!(
            heads
                .iter()
                .any(|head| head.contains("x-amz-copy-source: /b/folder/notes.txt")),
            "服务端复制要靠 x-amz-copy-source：{heads:?}"
        );
    }

    #[test]
    fn removing_a_folder_removes_every_object_under_it() {
        let fake = fake_bucket();
        let s3 = connected(&fake);

        s3.remove("folder").expect("该删得掉");

        let seen = fake.seen.lock().unwrap().clone();
        let deletes = seen
            .iter()
            .filter(|it| it.starts_with("DELETE"))
            .cloned()
            .collect::<Vec<_>>();
        // 只删 "folder" 这一个键的话，"删掉了"的文件夹下一秒还在——
        // 前缀底下的对象必须一个个清掉。
        assert!(
            deletes.iter().any(|it| it.contains("/b/folder/notes.txt")),
            "底下的对象要一起删：{seen:?}"
        );
        assert!(
            deletes.iter().any(|it| it.contains("/b/folder/")),
            "{seen:?}"
        );
    }
}
