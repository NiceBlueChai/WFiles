//! 单实例转发：第二次启动不自己开窗，把"要开到哪儿"交给已经开着的那一份。
//!
//! 为什么要有这一层：`wfiles ~/某个目录` 从脚本、从别的程序里被调起来时，
//! 用户要的是"那个窗口跳过去"，不是"再堆一扇一模一样的窗"。
//!
//! 但**不强制**单实例：转发不出去（对面没应答、门牌写不出来）就自己开窗。
//! 这条路上没有一种失败是"用户说的话被默默吞掉"——每一种都退到
//! "自己开一扇窗，开到他要的地方"。
//!
//! 三样东西凑成一件事：
//!
//! - **位子**（`instance.lock`）：谁拿到那把排他锁，谁就当主实例、负责听。
//!   锁是进程活着期间一直拿着的，进程没了系统自动放。**不做 pid 文件那套**
//!   ——那种锁在崩溃之后会留下一个假主人（文件还在、进程早没了），
//!   下一个启动会老老实实去敲一个不存在的门。
//! - **门牌**（`instance.json`）：监听端口写在这儿。第二次启动读它、连上去、
//!   说一句话、等一声"收到了"、退出。
//! - **那句话**：一行 ASCII 头（版本、开法、路径字节数）+ 路径的原始字节。
//!   路径**不经过字符串**（铁律 1）：Windows 上不是合法 UTF-8 的文件名
//!   是常态，走一趟 JSON 就会把那种路径变成"转发失败"。
//!
//! 版本对不上、门牌看不懂、对面没应答，一律走"自己开窗"。所以这套东西
//! 坏掉的后果只是"多一扇窗"，不会变成"点一下什么都没发生"。
//!
//! **`--lang` 不在可以代劳的范围内**：语言是这一扇窗口的属性，已经开着的
//! 窗口改不了。用户明说要换语言，就该由他自己开一扇新窗——转发过去等于
//! 把他那句话吞掉，而他下一次还会这么写（见 [`hand_off`]）。
//!
//! **两个文件都不删**：退出的那一份去删自己的门牌，恰好会删掉**新**主实例
//! 刚写下的那份（旧的正在退、新的刚好起来），而新主实例不会重写它——从此
//! 再也没人转得进来。锁文件同理：删掉一个别人正锁着的文件，下一个启动就能
//! 在**新的** inode 上抢到锁，于是同时有两个"主实例"。宁可留一张过期的纸：
//! 连不上自然走退路。

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cli::{self, Launch, Target};

/// 协议的版本。门牌里也带一份：对不上就不敲门（宁可自己开窗），
/// 免得拿新格式去敲旧实例的门。
const VERSION: u32 = 1;

/// 位子：排他锁。
const LOCK_FILE: &str = "instance.lock";
/// 门牌：监听端口。
const ENDPOINT_FILE: &str = "instance.json";

/// 头一行的开头，认得出这是谁在敲门。
const GREETING: &str = "wfiles-forward";
/// 头一行最多这么长。格式定死了就那么几十个字节，超出的只可能是恶意输入。
const HEADER_LIMIT: u64 = 128;
/// 路径长度上限：`PATH_MAX` 在三个平台上都不到 4 KiB，64 KiB 已经只剩恶意。
const PATH_LIMIT: usize = 64 * 1024;
/// 收到了。
const ACK: &[u8] = b"ok\n";

/// 连一次的上限。
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
/// 说一句话、等一声"收到了"的上限。
const TALK_TIMEOUT: Duration = Duration::from_millis(400);

/// 整趟转发的悬崖。
///
/// 主实例可能正开着：位子抢到了，门牌还没挂出去。给它这么长时间把门牌
/// 写出来；过了就自己开窗——别把用户晾在终端里等一个空承诺。
pub const HANDOFF_DEADLINE: Duration = Duration::from_millis(1500);
/// 两次尝试之间歇一下，再读一次门牌（对面可能刚把门牌写出来）。
const RETRY_PAUSE: Duration = Duration::from_millis(50);

/// 主实例的位子：拿着它的人负责听转发。
///
/// 锁活在这个句柄里，掉它=放锁。所以它得活着——界面上那个窗口开着多久，
/// 锁就该拿多久；听的那条线程被 `listen` 接着，锁也就跟着那条线程。
pub struct Claim {
    /// 门牌写在这儿（生产是状态目录，测试是临时目录）。
    dir: PathBuf,
    /// 光拿着不读不写，就为这把排他锁。
    _lock: std::fs::File,
}

/// 抢主实例的位子：抢到返回 `Some`，别人正拿着返回 `None`。
///
/// 拿不到位子不是错误——那说明已经有一份活着，这一次要么转发要么自己开窗，
/// 两种都不用听。状态目录都定不出来（没有家目录）就干脆没有这一层。
pub fn claim() -> Option<Claim> {
    claim_in(&crate::session::state_dir()?)
}

/// 同 [`claim`]，位子在哪儿由调用方给（测试用临时目录，生产用状态目录）。
fn claim_in(dir: &Path) -> Option<Claim> {
    std::fs::create_dir_all(dir).ok()?;
    // 不截断：这个文件里没有任何内容，它只是那把锁的载体。
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK_FILE))
        .ok()?;
    match lock.try_lock() {
        Ok(()) => Some(Claim {
            dir: dir.to_path_buf(),
            _lock: lock,
        }),
        // 抢不到：绝大多数时候是别人正拿着（已经有一份活着），也可能是
        // 这个文件系统不支持锁。两种都是"这一次不听"——别报错，别开不了窗。
        Err(_) => None,
    }
}

impl Claim {
    /// 挂出门牌、开始听，把收到的请求从通道那一头递出来。
    ///
    /// 绑不上端口、写不出门牌就返回 `None`：这一次不做转发，界面照开。
    /// 转发是顺手带的功能，不是用户开窗的前提。
    pub fn listen(self) -> Option<async_channel::Receiver<Launch>> {
        // 回环、随机端口：不占用户的端口，也不给外面的机器开口子。
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        write_endpoint(&self.dir, port).ok()?;
        // 无界通道：从"门牌挂出去"到"界面那头的消费任务起来"之间可能挤进
        // 几条请求，没有理由把它们丢掉。
        let (sender, receiver) = async_channel::unbounded();
        std::thread::spawn(move || {
            // 锁跟着这条线程活：线程在，门牌才是可信的。
            let _claim = self;
            serve(listener, sender);
        });
        Some(receiver)
    }
}

/// 收连接、读请求、交给界面那一头。
fn serve(listener: TcpListener, requests: async_channel::Sender<Launch>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            // 一条连不上的连接不代表这一份窗口坏了：接着听下一条。
            continue;
        };
        if !take_one(stream, &requests) && requests.is_closed() {
            // 没人接请求了（窗口关了）：门牌再挂着也没人应，收工。
            return;
        }
    }
}

/// 收下一条请求。`false` = 这一条没收成。
fn take_one(mut stream: TcpStream, requests: &async_channel::Sender<Launch>) -> bool {
    let _ = stream.set_read_timeout(Some(TALK_TIMEOUT));
    let _ = stream.set_write_timeout(Some(TALK_TIMEOUT));
    let Some(launch) = read_request(&mut stream) else {
        return false;
    };
    // 排队失败（界面那头没了）就不回话：对面会当成"没送到"，回去自己开窗。
    // 回一声"收到了"然后把请求丢掉，是最坏的一种答复。
    if requests.try_send(launch).is_err() {
        return false;
    }
    // 回的是"排上队了"，不是"办好了"：那件事得等界面线程来办，而说话的那
    // 一位说完就退出了，它也没法等结果。
    stream.write_all(ACK).is_ok() && stream.flush().is_ok()
}

/// 这一次启动有没有"可以请人代劳"的话。
fn handable(cli: &cli::Cli) -> Option<&Launch> {
    // `--lang` 是唯一一条不属于"可以代劳"的：语言是这一扇窗口的属性，
    // 已经开着的窗口改不了（理由见模块文档）。
    cli.launch.as_ref().filter(|_| cli.locale.is_none())
}

/// 把这一次启动说的话交给已经开着的那一份：送到了返回 `true`。
///
/// 送不到（对面正开着还没挂门牌、正在退、版本对不上）返回 `false`：
/// 调用方自己开窗就好——**别把用户敲的那条路径丢了**。
///
/// 会等 [`HANDOFF_DEADLINE`] 这么久：主实例可能刚起来，"位子抢到了、
/// 门牌还没写出来"那个空窗里不等一下就会白白多开一扇窗。
pub fn hand_off(cli: &cli::Cli) -> bool {
    let (Some(dir), Some(launch)) = (crate::session::state_dir(), handable(cli)) else {
        return false;
    };
    let handed = ask_around(&dir, launch, HANDOFF_DEADLINE);
    // 给排查看留一句：这一次启动没开窗，话交出去了。用户报"我跑了一下
    // 什么都没发生"时，这一行就是"送出去了没有"的答案。
    crate::logfile::info(&format!(
        "转发：{}「{}」",
        if handed {
            "交给已经开着的窗口"
        } else {
            "没送到，自己开窗"
        },
        launch.target.path().display()
    ));
    handed
}

/// 敲门：读门牌、连上、说一句、等一声"收到了"。没成就在 `budget` 里再来，
/// 每次重新读一遍门牌（对面可能刚把门牌写出来）。
fn ask_around(dir: &Path, launch: &Launch, budget: Duration) -> bool {
    let Some(body) = encode(launch) else {
        return false;
    };
    let deadline = Instant::now() + budget;
    loop {
        if let Some(port) = read_endpoint(dir)
            && let Ok(stream) = connect(port)
            && talk(stream, &body)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(RETRY_PAUSE);
    }
}

fn connect(port: u16) -> std::io::Result<TcpStream> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let stream = TcpStream::connect_timeout(&address.into(), CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(TALK_TIMEOUT))?;
    stream.set_write_timeout(Some(TALK_TIMEOUT))?;
    Ok(stream)
}

/// 说一句，等一声"收到了"。
fn talk(mut stream: TcpStream, body: &[u8]) -> bool {
    if stream.write_all(body).is_err() {
        return false;
    }
    // 半关写端：对面读到 EOF 就知道话说完了（长度已经写在头里，这一步
    // 只是让"话说不完整"在对面立刻失败，不用等超时）。
    let _ = stream.shutdown(Shutdown::Write);
    let mut reply = [0u8; ACK.len()];
    stream.read_exact(&mut reply).is_ok() && reply == ACK
}

/// 门牌。
#[derive(Serialize, Deserialize)]
struct Endpoint {
    /// 协议版本：对不上就不敲这个门。
    version: u32,
    /// 监听端口（本机回环）。
    port: u16,
}

/// 把门牌挂出去。
///
/// **只由拿着位子的那一份写**，所以不用抢写、也没有并发：一个状态目录里
/// 同时最多一个主实例。
fn write_endpoint(dir: &Path, port: u16) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_string(&Endpoint {
        version: VERSION,
        port,
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(dir.join(ENDPOINT_FILE), body)
}

/// 读门牌。读不出来、看不懂、版本对不上，一律当作"没有"。
fn read_endpoint(dir: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(dir.join(ENDPOINT_FILE)).ok()?;
    let endpoint: Endpoint = serde_json::from_str(&text).ok()?;
    (endpoint.version == VERSION).then_some(endpoint.port)
}

/// 把一次启动编成字节：一行 ASCII 头 + 路径的原始字节。
///
/// 路径不经过字符串（铁律 1）。真传不过去的（Windows 上带孤立代理项的
/// 名字）也不硬编：返回 `None`，调用方自己开窗。
fn encode(launch: &Launch) -> Option<Vec<u8>> {
    let path = path_bytes(launch.target.path())?;
    if path.len() > PATH_LIMIT {
        return None;
    }
    let mode = match launch.target {
        Target::Open(_) => "open",
        Target::Reveal(_) => "reveal",
    };
    let tab = if launch.new_tab { "tab" } else { "here" };
    let mut body = format!("{GREETING} {VERSION} {mode} {tab} {}\n", path.len()).into_bytes();
    body.extend_from_slice(&path);
    Some(body)
}

/// 解开一次启动。看不懂就不是给我们的请求。
fn decode(body: &[u8]) -> Option<Launch> {
    let cut = body.iter().position(|&byte| byte == b'\n')?;
    let head = parse_head(&body[..cut])?;
    // 声明了几个字节就得是几个字节：多出来的那截不是我们写的东西。
    if body.len() - cut - 1 != head.length {
        return None;
    }
    let path = path_from_bytes(&body[cut + 1..])?;
    Some(Launch {
        target: if head.reveal {
            Target::Reveal(path)
        } else {
            Target::Open(path)
        },
        new_tab: head.new_tab,
    })
}

/// 头一行的意思。
struct Head {
    reveal: bool,
    new_tab: bool,
    /// 后面还跟着多少个字节的路径。
    length: usize,
}

/// 解析头一行：字段、顺序、个数都按我们写的那样认，多一个少一个都不是。
fn parse_head(line: &[u8]) -> Option<Head> {
    let line = std::str::from_utf8(line).ok()?;
    let mut fields = line.split(' ');
    if fields.next()? != GREETING {
        return None;
    }
    if fields.next()?.parse::<u32>().ok()? != VERSION {
        return None;
    }
    let reveal = match fields.next()? {
        "open" => false,
        "reveal" => true,
        _ => return None,
    };
    let new_tab = match fields.next()? {
        "here" => false,
        "tab" => true,
        _ => return None,
    };
    let length: usize = fields.next()?.parse().ok()?;
    if length > PATH_LIMIT || fields.next().is_some() {
        return None;
    }
    Some(Head {
        reveal,
        new_tab,
        length,
    })
}

/// 从流里读一条请求。
fn read_request(stream: &mut TcpStream) -> Option<Launch> {
    let mut reader = std::io::BufReader::new(stream);
    // 头一行有长度上限：对面不发换行的话，`read_until` 会一路读到内存
    // 见底——那是伺候恶意输入，不是伺候用户。
    let mut line = Vec::new();
    (&mut reader)
        .take(HEADER_LIMIT)
        .read_until(b'\n', &mut line)
        .ok()?;
    let head = parse_head(line.strip_suffix(b"\n")?)?;
    // 长度是头里说了算的：正好读这么多字节走，多一个少一个都不认。
    let mut path = vec![0u8; head.length];
    reader.read_exact(&mut path).ok()?;
    line.extend_from_slice(&path);
    decode(&line)
}

/// 路径 → 原样的字节。
///
/// Unix 上就是原字节；Windows 上走 UTF-8（那边路径的存储形态是宽字符，
/// `to_str` 只对带孤立代理项的名字失败），失败就不转发、自己开窗。
#[cfg(unix)]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;
    Some(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Option<Vec<u8>> {
    Some(path.to_str()?.as_bytes().to_vec())
}

/// 原样的字节 → 路径。
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    Some(PathBuf::from(std::str::from_utf8(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(target: Target, new_tab: bool) -> Launch {
        Launch { target, new_tab }
    }

    fn cli_of(launch: Option<Launch>, locale: Option<crate::i18n::Locale>) -> cli::Cli {
        cli::Cli {
            locale,
            launch,
            new_window: false,
            problem: None,
        }
    }

    #[test]
    fn a_request_survives_the_wire() {
        // 四种组合都走一遍：开法两位、开不开新栏两位。
        for target in [
            Target::Open(PathBuf::from("/tmp/某个目录")),
            Target::Reveal(PathBuf::from("/tmp/某份 报告.txt")),
        ] {
            for new_tab in [false, true] {
                let asked = launch(target.clone(), new_tab);
                let body = encode(&asked).expect("该编得出来");
                assert_eq!(decode(&body), Some(asked), "编了又解该是同一句话");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_path_travels_as_bytes_not_as_text() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
        // 铁律 1：路径按字节走。不是合法 UTF-8 的名字是常态，
        // 过一趟字符串那种名字就没了。
        let raw = std::ffi::OsString::from_vec(b"/tmp/\xff\xfe".to_vec());
        let asked = launch(Target::Open(PathBuf::from(raw)), false);
        let body = encode(&asked).expect("该编得出来");
        let back = decode(&body).expect("该解得回来");
        assert_eq!(
            back.target.path().as_os_str().as_bytes(),
            b"/tmp/\xff\xfe",
            "字节进字节出，一个都不许动"
        );
    }

    #[test]
    fn a_mangled_request_is_not_a_request() {
        // 每一种都能在真机上碰到：别的程序占了那个端口、旧版本写的门牌、
        // 半截连接。它们**都不该**被当成一次启动。
        let good = encode(&launch(Target::Open(PathBuf::from("/tmp/x")), false)).unwrap();
        let bad: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"hello\n".to_vec(),
            // 认得出开头，版本不是我们这一版
            b"wfiles-forward 99 open here 6\n/tmp/x".to_vec(),
            // 开法不认识
            b"wfiles-forward 1 open-please here 6\n/tmp/x".to_vec(),
            // 栏数不认识
            b"wfiles-forward 1 open maybe 6\n/tmp/x".to_vec(),
            // 长度对不上：说要 6 字节，给了 5 字节
            b"wfiles-forward 1 open here 6\n/tmp/".to_vec(),
            // 长度对不上：说要 5 字节，多给了一个
            b"wfiles-forward 1 open here 5\n/tmp/xx".to_vec(),
            // 多一个字段
            b"wfiles-forward 1 open here 6 extra\n/tmp/x".to_vec(),
            // 头里没有换行
            b"wfiles-forward 1 open here 6".to_vec(),
            // 长度大得离谱（理论上要 4 GB 内存）
            b"wfiles-forward 1 open here 999999999999\n".to_vec(),
        ];
        for body in bad {
            assert_eq!(decode(&body), None, "这种不该认：{body:?}");
        }
        assert!(decode(&good).is_some(), "正常的还得认得出来");
    }

    #[test]
    fn only_these_two_things_are_worth_handing_over() {
        // 没有目标：没话可说（打开一个窗口这件事没法"代劳"。
        assert!(handable(&cli_of(None, None)).is_none());
        // `--lang`：语言是这一扇窗口的属性，开着的窗口改不了。
        let asked = launch(Target::Open(PathBuf::from("/tmp/x")), false);
        assert!(
            handable(&cli_of(Some(asked.clone()), Some(crate::i18n::Locale::En))).is_none(),
            "说了要换语言，就该由用户自己开一扇新窗"
        );
        assert_eq!(
            handable(&cli_of(Some(asked.clone()), None)),
            Some(&asked),
            "既给了目标又没提语言：这一句可以请人代劳"
        );
    }

    #[test]
    fn a_second_launch_reaches_the_one_that_is_listening() {
        let dir = tempfile::tempdir().unwrap();
        let claim = claim_in(dir.path()).expect("空目录里该抢得到位子");
        let requests = claim.listen().expect("该绑得上端口");
        // 门牌挂出去了：别人才找得着。
        assert!(read_endpoint(dir.path()).is_some(), "门牌该挂出去了");

        let asked = launch(Target::Reveal(PathBuf::from("/tmp/某份 报告.txt")), true);
        let handed = ask_around(dir.path(), &asked, Duration::from_millis(200));
        assert!(handed, "走一趟真的 socket：该送到了");
        // 回话是在"排进队列"之后才发的，所以这里不用等——一定在。
        assert_eq!(
            requests.try_recv().ok(),
            Some(asked),
            "送到的那句话该一字不差"
        );
    }

    #[test]
    fn a_launch_nobody_answers_falls_back_to_opening_its_own() {
        let dir = tempfile::tempdir().unwrap();
        // 门牌还在（上一份退得急），但端口上已经没人听了——
        // 真的拿一个空端口：绑上又马上放开，回环上没人能抢得这么快。
        let port = {
            let spare = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
            spare.local_addr().unwrap().port()
        };
        write_endpoint(dir.path(), port).unwrap();

        let asked = launch(Target::Open(PathBuf::from("/tmp/x")), false);
        assert!(
            !ask_around(dir.path(), &asked, Duration::from_millis(120)),
            "没人应答就该说自己没送到，让调用方自己开窗"
        );
    }

    #[test]
    fn a_second_claim_fails_while_the_first_one_holds_the_seat() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim_in(dir.path()).expect("空目录里该抢得到位子");
        assert!(
            claim_in(dir.path()).is_none(),
            "位子只有一个：还拿着的时候别人该抢不到"
        );
        drop(first);
        assert!(claim_in(dir.path()).is_some(), "放手之后该轮得到下一份");
    }

    #[test]
    fn an_endpoint_from_another_version_is_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(ENDPOINT_FILE),
            format!(r#"{{"version": {}, "port": 12345}}"#, VERSION + 1),
        )
        .unwrap();
        assert_eq!(read_endpoint(dir.path()), None, "格式换过的门牌不该照着敲");
        std::fs::write(dir.path().join(ENDPOINT_FILE), "半个 {").unwrap();
        assert_eq!(read_endpoint(dir.path()), None, "坏门牌当没有");
    }
}
