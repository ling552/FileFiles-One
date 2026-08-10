//! 后台文件任务：复制 / 移动 / 解压 / 压缩的异步执行，带真实进度、暂停与取消。
//!
//! 设计要点：工作线程只持有 `Arc<TaskControl>`（可跨线程）与一个进度回调闭包，
//! 不接触 `Rc<RefCell<AppCore>>`（非 Send）。进度通过回调里的
//! `slint::invoke_from_event_loop` 回到主线程刷新 UI，完成后由主线程的
//! `task-finished` 回调负责重载目录并启动队列中的下一项。

use super::metadata::{fmt_ts_full, human_size, unix_ts};
use super::operations::{is_archive, resolve_conflict, ArchiveFormat};
use std::cell::Cell;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 任务类型
#[derive(Clone, Copy, PartialEq)]
pub enum TaskKind {
    Copy,
    Move,
    /// 解压归档：srcs 为归档文件（入队时逐归档拆分为单独任务），dst 为解压目标目录
    Extract,
    /// 压缩归档：srcs 为待压缩项，dst 为归档输出完整路径
    /// （入队时已避让重名，输出格式按 dst 扩展名识别）
    Compress,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Copy => "复制文件",
            TaskKind::Move => "移动文件",
            TaskKind::Extract => "解压文件",
            TaskKind::Compress => "压缩文件",
        }
    }
}

/// 一个待执行的任务。`dst` 的含义随任务类型不同：
/// Copy/Move 为目标目录，Extract 为解压目标目录，Compress 为归档输出完整路径。
pub struct Job {
    pub kind: TaskKind,
    pub srcs: Vec<PathBuf>,
    pub dst: PathBuf,
}

/// 同名冲突时用户的处置方式
#[derive(Clone, Copy, PartialEq)]
pub enum ConflictDecision {
    /// 覆盖已存在的目标（先删除目标再写入）
    Overwrite,
    /// 跳过该项，保留目标不动
    Skip,
    /// 保留两者：把新项自动重命名为「名称 (2)」
    Rename,
}

/// 工作线程 → 主线程的冲突询问：携带展示对话框所需的信息
pub struct ConflictQuery {
    pub name: String,      // 冲突项名称
    pub operation: String, // 操作标签（「复制文件」/「移动文件」）
    pub src_info: String,  // 源：大小 · 修改日期
    pub dst_info: String,  // 目标：大小 · 修改日期
    pub is_dir: bool,      // 冲突项是否为文件夹
}

/// 主线程 → 工作线程的冲突处置回复
pub struct ConflictReply {
    pub decision: ConflictDecision,
    /// 是否将该处置应用到本任务后续所有冲突（不再逐个询问）
    pub apply_all: bool,
}

/// 冲突处理结果
enum ConflictOutcome {
    /// 无冲突，或 Overwrite 删目标 / Rename 改名后可继续操作
    Proceed,
    /// 用户选择跳过该项
    Skip,
}

/// 冲突对话框桥：主线程与工作线程共享。工作线程遇冲突时把一个回复通道存入
/// `pending` 并请主线程弹窗；用户选择后主线程取出通道回送 `ConflictReply`。
#[derive(Default)]
pub struct ConflictBridge {
    pub pending: Mutex<Option<Sender<ConflictReply>>>,
}

impl ConflictBridge {
    pub fn new() -> Self {
        Self::default()
    }
}

/// 暂停 / 取消控制位，工作线程与 UI 线程共享
pub struct TaskControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl TaskControl {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }

    /// 设置暂停状态，返回新状态
    pub fn toggle_pause(&self) -> bool {
        let next = !self.paused.load(Ordering::Relaxed);
        self.paused.store(next, Ordering::Relaxed);
        next
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 处于暂停时自旋等待（每 80ms 检查一次），被取消则立即返回
    fn wait_if_paused(&self) {
        while self.paused.load(Ordering::Relaxed) && !self.is_cancelled() {
            std::thread::sleep(Duration::from_millis(80));
        }
    }
}

/// 进度快照，回填到 UI 的 ProgressCard
#[derive(Clone, Default)]
pub struct Progress {
    pub operation: String,
    pub current_file: String,
    pub target: String,
    pub completed: i32,
    pub total: i32,
    pub fraction: f32,
    pub speed: String,
    pub eta: String,
}

/// 任务结果
pub struct TaskResult {
    pub ok: i32,
    /// 因同名冲突被用户选择「跳过」的项数
    pub skipped: i32,
    pub error: String,
    pub cancelled: bool,
    /// 成功完成的目标路径列表（用于完成后自动选中）
    pub completed_paths: Vec<PathBuf>,
}

/// 在工作线程中执行任务。
/// `report` 用于回送进度（每次约 ≥40ms 一帧）；
/// `ask` 在遇到顶层同名冲突时被调用，阻塞直到用户在主线程做出处置选择。
pub fn run(
    job: Job,
    ctrl: Arc<TaskControl>,
    report: impl Fn(Progress),
    ask: impl Fn(ConflictQuery) -> ConflictReply,
) -> TaskResult {
    // 任一端落在便携设备上：std::fs 不可用，整体改走 WPD 传输
    if matches!(job.kind, TaskKind::Copy | TaskKind::Move) && job_touches_device(&job) {
        return run_mtp(job, ctrl, report, ask);
    }

    let op = job.kind.label();
    let target = job.dst.to_string_lossy().to_string();
    // 总量统计：解压按归档条目表（tar 系无法便宜预知，退化为压缩输入字节，
    // 文件总数用 -1 表示未知）；其余按源递归扫描
    let (total_files, total_bytes) = if job.kind == TaskKind::Extract {
        archive_totals(&job.srcs)
    } else {
        scan(&job.srcs)
    };

    let mut runner = Runner {
        ctrl: &ctrl,
        report: &report,
        ask: &ask,
        op,
        target: &target,
        total_files,
        total_bytes,
        done_files: 0,
        done_bytes: 0,
        skipped: 0,
        start: Instant::now(),
        last_emit: Instant::now() - Duration::from_secs(1),
        remembered: None,
    };

    // 初始帧：让卡片立即出现并显示总数
    runner.emit("准备中…", true);

    match job.kind {
        TaskKind::Extract => return runner.run_extract(&job),
        TaskKind::Compress => return runner.run_compress(&job),
        TaskKind::Copy | TaskKind::Move => {}
    }

    let mut ok = 0;
    let mut error = String::new();
    let mut completed_paths = Vec::new();
    for src in &job.srcs {
        if ctrl.is_cancelled() {
            return TaskResult {
                ok,
                skipped: runner.skipped,
                error,
                cancelled: true,
                completed_paths,
            };
        }
        let file_name = match src.file_name() {
            Some(n) => n,
            None => continue,
        };
        let dest = job.dst.join(file_name);

        // 顶层同名冲突：沿用已记忆的决策，或请主线程弹窗询问用户。
        // decide 内部处理 Overwrite（删除目标）/ Rename（改名）/ Skip。
        let (outcome, dest) = runner.decide(src, &dest);
        if matches!(outcome, ConflictOutcome::Skip) {
            // 跳过项计入进度，避免 fraction 卡在不到 100%
            runner.count_skipped(src);
            runner.skipped += 1;
            continue;
        }

        let res = if job.kind == TaskKind::Move {
            runner.move_one(src, &dest)
        } else {
            runner.copy_one(src, &dest)
        };
        match res {
            Ok(true) => {
                ok += 1;
                completed_paths.push(dest.clone());
            }
            Ok(false) => {
                // 被取消
                return TaskResult {
                    ok,
                    skipped: runner.skipped,
                    error,
                    cancelled: true,
                    completed_paths,
                };
            }
            Err(e) => {
                error = e.to_string();
            }
        }
    }

    TaskResult {
        ok,
        skipped: runner.skipped,
        error,
        cancelled: false,
        completed_paths,
    }
}

/// 构造冲突询问信息（名称、操作、源与目标的大小/日期对比）
fn build_query(src: &Path, dest: &Path, op: &str) -> ConflictQuery {
    ConflictQuery {
        name: name_of(dest),
        operation: op.to_string(),
        src_info: describe(src),
        dst_info: describe(dest),
        is_dir: dest.is_dir(),
    }
}

/// 生成「大小 · 修改日期」描述串（文件夹的大小显示为「文件夹」）
fn describe(p: &Path) -> String {
    match fs::metadata(p) {
        Ok(m) => {
            let size = if m.is_dir() {
                "文件夹".to_string()
            } else {
                human_size(m.len())
            };
            let ts = m.modified().ok().map(unix_ts).unwrap_or(0);
            format!("{} · {}", size, fmt_ts_full(ts))
        }
        Err(_) => "信息不可用".to_string(),
    }
}

/// 递归统计待处理的文件数与字节总量（用于进度百分比与 ETA）
fn scan(srcs: &[PathBuf]) -> (i32, u64) {
    let mut files = 0i32;
    let mut bytes = 0u64;
    for s in srcs {
        scan_one(s, &mut files, &mut bytes);
    }
    (files, bytes)
}

fn scan_one(path: &Path, files: &mut i32, bytes: &mut u64) {
    if path.is_dir() {
        if let Ok(rd) = fs::read_dir(path) {
            for ent in rd.flatten() {
                scan_one(&ent.path(), files, bytes);
            }
        }
    } else {
        *files += 1;
        *bytes += fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
}

/// 统计解压任务的总量：zip/7z 读条目表（快速，字节为解压后大小）；
/// tar/tar.gz 无法便宜预知解压后大小，改按压缩输入字节计
/// （bytes = 归档文件大小，与 CountingReader 的读取偏移配套），文件总数返回 -1 表示未知。
fn archive_totals(srcs: &[PathBuf]) -> (i32, u64) {
    let mut files = 0i64;
    let mut bytes = 0u64;
    let mut unknown_files = false;
    for s in srcs {
        let by_input_size = |bytes: &mut u64, unknown: &mut bool| {
            *bytes += fs::metadata(s).map(|m| m.len()).unwrap_or(0);
            *unknown = true;
        };
        match is_archive(s) {
            Some(ArchiveFormat::Zip) => {
                let scanned = fs::File::open(s).ok().and_then(|f| {
                    let mut zip = zip::ZipArchive::new(f).ok()?;
                    for i in 0..zip.len() {
                        if let Ok(e) = zip.by_index_raw(i) {
                            if !e.is_dir() {
                                files += 1;
                                bytes += e.size();
                            }
                        }
                    }
                    Some(())
                });
                if scanned.is_none() {
                    by_input_size(&mut bytes, &mut unknown_files);
                }
            }
            Some(ArchiveFormat::SevenZ) => {
                match sevenz_rust::SevenZReader::open(s, sevenz_rust::Password::empty()) {
                    Ok(sz) => {
                        for e in sz.archive().files.iter() {
                            if !e.is_directory() {
                                files += 1;
                                bytes += e.size;
                            }
                        }
                    }
                    Err(_) => by_input_size(&mut bytes, &mut unknown_files),
                }
            }
            // tar / tar.gz / 无法识别：按压缩输入字节
            _ => by_input_size(&mut bytes, &mut unknown_files),
        }
    }
    let files = if unknown_files {
        -1
    } else {
        files.min(i32::MAX as i64) as i32
    };
    (files, bytes)
}

/// 清洗归档内条目的相对路径：仅保留普通组件，拒绝绝对路径、盘符与 `..`
/// （路径穿越防护），空路径返回 None（该条目应被跳过）。
fn sanitize_rel_path(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::Normal(c) => out.push(c),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 压缩清单条目：目录（含空目录，携带磁盘路径供 tar 读元数据）或文件。
/// 归档内相对路径统一用 '/' 分隔。
enum PackItem {
    Dir(PathBuf, String),
    File(PathBuf, String),
}

/// 展开待压缩源为归档条目清单（目录递归，保留相对结构）
fn walk_pack(srcs: &[PathBuf]) -> Vec<PackItem> {
    let mut out = Vec::new();
    for s in srcs {
        let base = match s.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        if s.is_dir() {
            walk_pack_dir(s, &base, &mut out);
        } else {
            out.push(PackItem::File(s.clone(), base));
        }
    }
    out
}

fn walk_pack_dir(dir: &Path, prefix: &str, out: &mut Vec<PackItem>) {
    out.push(PackItem::Dir(dir.to_path_buf(), prefix.to_string()));
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            let rel = format!("{}/{}", prefix, ent.file_name().to_string_lossy());
            if p.is_dir() {
                walk_pack_dir(&p, &rel, out);
            } else {
                out.push(PackItem::File(p, rel));
            }
        }
    }
}

/// 包装读取流并统计已读字节：tar/tar.gz 解压以压缩输入偏移作进度。
struct CountingReader<R: Read> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get() + n as u64);
        Ok(n)
    }
}

/// 包装读取流：每次 read 前响应暂停，取消时返回错误以中断上层库的内部拷贝
/// （tar/7z 压缩把数据流交给库内部消费，无法在外层逐块检查）。
struct GatedReader<'c, R: Read> {
    inner: R,
    ctrl: &'c TaskControl,
}

impl<R: Read> Read for GatedReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.ctrl.wait_if_paused();
        if self.ctrl.is_cancelled() {
            // 不用 Interrupted：std::io::copy 会对 Interrupted 自动重试导致死循环
            return Err(io::Error::new(io::ErrorKind::Other, "任务已取消"));
        }
        self.inner.read(buf)
    }
}

struct Runner<'a, F: Fn(Progress), G: Fn(ConflictQuery) -> ConflictReply> {
    ctrl: &'a TaskControl,
    report: &'a F,
    ask: &'a G,
    op: &'static str,
    target: &'a str,
    total_files: i32,
    total_bytes: u64,
    done_files: i32,
    done_bytes: u64,
    /// 因冲突被用户跳过的项数（计入完成消息的「跳过 N 项」）
    skipped: i32,
    start: Instant,
    last_emit: Instant,
    /// 「应用到后续全部」选中后记忆的决策，子项冲突复用以避免逐个询问
    remembered: Option<ConflictDecision>,
}

impl<'a, F: Fn(Progress), G: Fn(ConflictQuery) -> ConflictReply> Runner<'a, F, G> {
    /// 发送一帧进度。`force` 为真时忽略节流（用于首帧、整文件完成等关键节点）。
    fn emit(&mut self, current: &str, force: bool) {
        if !force && self.last_emit.elapsed() < Duration::from_millis(40) {
            return;
        }
        self.last_emit = Instant::now();
        (self.report)(make_progress(
            self.op,
            current,
            self.target,
            self.done_files,
            self.total_files,
            self.done_bytes,
            self.total_bytes,
            self.start,
        ));
    }

    /// 冲突处理：返回 (结果, 可能重命名后的目标路径)。
    /// - 无冲突 / Overwrite 删目标后 / Rename 改名后：Proceed
    /// - Skip：跳过该项
    /// 子项目录冲突由 copy_tree 直接合并（不走本方法）；本方法用于顶层项与子项文件。
    fn decide(&mut self, src: &Path, dest: &Path) -> (ConflictOutcome, PathBuf) {
        if !dest.exists() {
            return (ConflictOutcome::Proceed, dest.to_path_buf());
        }
        let query = build_query(src, dest, self.op);
        self.resolve_decision(dest, query)
    }

    /// 归档条目版冲突询问：源在归档内、磁盘上没有对应文件，源信息按条目大小构造。
    fn decide_extract(&mut self, dest: &Path, size: u64) -> (ConflictOutcome, PathBuf) {
        if !dest.exists() {
            return (ConflictOutcome::Proceed, dest.to_path_buf());
        }
        let query = ConflictQuery {
            name: name_of(dest),
            operation: self.op.to_string(),
            src_info: format!("{} · 归档内条目", human_size(size)),
            dst_info: describe(dest),
            is_dir: dest.is_dir(),
        };
        self.resolve_decision(dest, query)
    }

    /// 取用户（或已记忆）的冲突决策并执行 Overwrite/Rename 的前置动作
    fn resolve_decision(
        &mut self,
        dest: &Path,
        query: ConflictQuery,
    ) -> (ConflictOutcome, PathBuf) {
        let decision = match self.remembered {
            Some(d) => d,
            None => {
                let reply = (self.ask)(query);
                if reply.apply_all {
                    self.remembered = Some(reply.decision);
                }
                reply.decision
            }
        };
        match decision {
            ConflictDecision::Skip => (ConflictOutcome::Skip, dest.to_path_buf()),
            ConflictDecision::Rename => (
                ConflictOutcome::Proceed,
                resolve_conflict(dest.to_path_buf()),
            ),
            ConflictDecision::Overwrite => {
                // 删除已存在目标（文件 remove_file / 目录 remove_dir_all）后继续
                if dest.is_dir() {
                    let _ = fs::remove_dir_all(dest);
                } else {
                    let _ = fs::remove_file(dest);
                }
                (ConflictOutcome::Proceed, dest.to_path_buf())
            }
        }
    }

    /// 以当前 done_files/skipped 汇总任务结果（解压/压缩路径用）。
    /// done_files 为进度计数（含跳过项），ok 需扣除跳过数。
    fn result(&self, cancelled: bool, error: String) -> TaskResult {
        TaskResult {
            ok: (self.done_files - self.skipped).max(0),
            skipped: self.skipped,
            error,
            cancelled,
            completed_paths: Vec::new(),
        }
    }

    /// 汇总任务结果（复制/移动路径用：ok 按顶层源计数而非文件数）
    fn result_with(&self, ok: i32, error: String, cancelled: bool) -> TaskResult {
        TaskResult {
            ok,
            skipped: self.skipped,
            error,
            cancelled,
            completed_paths: Vec::new(),
        }
    }

    /// 跳过的项计入进度（文件数 + 字节数），避免进度条卡在不到 100%。
    fn count_skipped(&mut self, src: &Path) {
        let mut f = 0i32;
        let mut b = 0u64;
        scan_one(src, &mut f, &mut b);
        self.done_files += f;
        self.done_bytes += b;
        self.emit(&name_of(src), false);
    }

    /// 复制单个源（文件或目录）。返回 Ok(false) 表示中途被取消。
    fn copy_one(&mut self, src: &Path, dest: &Path) -> io::Result<bool> {
        if src.is_dir() {
            self.copy_tree(src, dest)
        } else {
            self.copy_file(src, dest)
        }
    }

    /// 移动单个源：先尝试同盘 rename（瞬时），失败则跨盘复制后删除源。
    fn move_one(&mut self, src: &Path, dest: &Path) -> io::Result<bool> {
        match fs::rename(src, dest) {
            Ok(_) => {
                // rename 瞬时完成、不产生逐字节进度，按目标整体大小一次性记入
                let mut f = 0;
                let mut b = 0;
                scan_one(dest, &mut f, &mut b);
                self.done_files += f;
                self.done_bytes += b;
                self.emit(&name_of(dest), true);
                Ok(true)
            }
            Err(_) => {
                // 跨盘：复制后删除
                let copied = self.copy_one(src, dest)?;
                if !copied {
                    return Ok(false);
                }
                if src.is_dir() {
                    let _ = fs::remove_dir_all(src);
                } else {
                    let _ = fs::remove_file(src);
                }
                Ok(true)
            }
        }
    }

    fn copy_tree(&mut self, src: &Path, dest: &Path) -> io::Result<bool> {
        fs::create_dir_all(dest)?;
        for ent in fs::read_dir(src)? {
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            let ent = ent?;
            let from = ent.path();
            let to = dest.join(ent.file_name());
            let done = if from.is_dir() {
                self.copy_tree(&from, &to)?
            } else {
                self.copy_file(&from, &to)?
            };
            if !done {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 分块复制单个文件（1 MiB/块），逐块更新进度、响应暂停与取消。
    fn copy_file(&mut self, from: &Path, to: &Path) -> io::Result<bool> {
        // 子项文件冲突检查：目录冲突由 copy_tree 直接合并，仅文件级走 decide
        let (outcome, to) = self.decide(from, to);
        if matches!(outcome, ConflictOutcome::Skip) {
            self.count_skipped(from);
            self.skipped += 1;
            return Ok(true);
        }
        let name = name_of(from);
        let mut reader = fs::File::open(from)?;
        let mut writer = fs::File::create(&to)?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                drop(writer);
                let _ = fs::remove_file(&to); // 清理半成品
                return Ok(false);
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            self.done_bytes += n as u64;
            self.emit(&name, false);
        }
        self.done_files += 1;
        self.emit(&name, true);
        Ok(true)
    }

    // ──────────────────────── 解压 ────────────────────────

    /// 解压任务主流程：逐归档解压到 job.dst，逐条目上报进度、响应暂停/取消、
    /// 目标同名文件走冲突询问。
    fn run_extract(&mut self, job: &Job) -> TaskResult {
        let mut completed_paths = Vec::new();
        for archive in &job.srcs {
            if self.ctrl.is_cancelled() {
                return TaskResult {
                    ok: 0,
                    skipped: self.skipped,
                    error: String::new(),
                    cancelled: true,
                    completed_paths,
                };
            }
            // 记录解压前目标目录中已存在的条目，用于区分解压新增的条目
            let before: std::collections::HashSet<_> = if job.dst.exists() {
                std::fs::read_dir(&job.dst)
                    .ok()
                    .map(|rd| {
                        rd.filter_map(Result::ok)
                            .map(|e| e.path())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                std::collections::HashSet::new()
            };

            let res = match is_archive(archive) {
                Some(ArchiveFormat::Zip) => self.extract_zip(archive, &job.dst),
                Some(ArchiveFormat::SevenZ) => self.extract_7z(archive, &job.dst),
                Some(ArchiveFormat::Tar) => self.extract_tar(archive, &job.dst, false),
                Some(ArchiveFormat::TarGz) => self.extract_tar(archive, &job.dst, true),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "不支持的归档格式",
                )),
            };
            match res {
                Ok(true) => {
                    // 收集解压后新增的顶层条目
                    if let Ok(rd) = std::fs::read_dir(&job.dst) {
                        for entry in rd.filter_map(Result::ok) {
                            let p = entry.path();
                            if !before.contains(&p) {
                                completed_paths.push(p);
                            }
                        }
                    }
                }
                Ok(false) => {
                    return TaskResult {
                        ok: 0,
                        skipped: self.skipped,
                        error: String::new(),
                        cancelled: true,
                        completed_paths,
                    };
                }
                Err(e) => {
                    return TaskResult {
                        ok: 0,
                        skipped: self.skipped,
                        error: format!("解压失败：{}", e),
                        cancelled: false,
                        completed_paths,
                    };
                }
            }
        }
        self.emit("完成", true);
        TaskResult {
            ok: self.done_files as i32,
            skipped: self.skipped,
            error: String::new(),
            cancelled: false,
            completed_paths,
        }
    }

    /// 逐条目解压 ZIP：enclosed_name 防路径穿越，文件级分块写入并上报进度。
    fn extract_zip(&mut self, archive: &Path, dst_dir: &Path) -> io::Result<bool> {
        let file = fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("无法读取归档：{}", e))
        })?;
        fs::create_dir_all(dst_dir)?;

        for i in 0..zip.len() {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            let mut entry = zip
                .by_index(i)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            // 防路径穿越：仅接受能安全限定在目标目录内的相对路径
            let rel = match entry.enclosed_name() {
                Some(p) => p,
                None => continue,
            };
            let outpath = dst_dir.join(rel);
            if entry.is_dir() {
                fs::create_dir_all(&outpath)?;
                continue;
            }
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)?;
            }
            let size = entry.size();
            let (outcome, outpath) = self.decide_extract(&outpath, size);
            if matches!(outcome, ConflictOutcome::Skip) {
                self.done_files += 1;
                self.done_bytes += size;
                self.skipped += 1;
                continue;
            }
            if !self.write_stream(&mut entry, &outpath)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 逐条目解压 7z：for_each_entries 顺序解码，路径经 sanitize 防穿越。
    /// 注意：同一压缩块（solid block）内条目为连续流，任何被跳过的条目
    /// （冲突 Skip 或路径不安全）都必须读完其数据，否则后续条目全部错位。
    fn extract_7z(&mut self, archive: &Path, dst_dir: &Path) -> io::Result<bool> {
        fs::create_dir_all(dst_dir)?;
        let mut sz = sevenz_rust::SevenZReader::open(archive, sevenz_rust::Password::empty())
            .map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("无法读取归档：{}", e))
            })?;
        let mut cancelled = false;
        let result = sz.for_each_entries(|entry, reader| {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                cancelled = true;
                return Ok(false); // 返回 false 终止遍历
            }
            if entry.is_directory() {
                if let Some(rel) = sanitize_rel_path(Path::new(entry.name())) {
                    fs::create_dir_all(dst_dir.join(rel))?;
                }
                return Ok(true);
            }
            let name = name_of(Path::new(entry.name()));
            let rel = match sanitize_rel_path(Path::new(entry.name())) {
                Some(p) => p,
                None => {
                    // 路径不安全：排空数据保持流对齐，计入进度但不落盘
                    if !self.drain_stream(reader, &name)? {
                        cancelled = true;
                        return Ok(false);
                    }
                    self.done_files += 1;
                    self.skipped += 1;
                    return Ok(true);
                }
            };
            let outpath = dst_dir.join(rel);
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)?;
            }
            let (outcome, outpath) = self.decide_extract(&outpath, entry.size);
            if matches!(outcome, ConflictOutcome::Skip) {
                // 读完并丢弃：7z 同块内后续条目依赖流位置连续
                if !self.drain_stream(reader, &name)? {
                    cancelled = true;
                    return Ok(false);
                }
                self.done_files += 1;
                self.skipped += 1;
                return Ok(true);
            }
            if !self.write_stream(reader, &outpath)? {
                cancelled = true;
                return Ok(false);
            }
            Ok(true)
        });
        match result {
            Ok(_) => Ok(!cancelled),
            Err(sevenz_rust::Error::Io(e, _)) => Err(e),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, format!("7z：{}", e))),
        }
    }

    /// 分块读完并丢弃一个条目流：逐块响应暂停/取消并计入字节进度。
    /// 返回 Ok(false) 表示中途被取消。
    fn drain_stream(&mut self, reader: &mut dyn Read, name: &str) -> io::Result<bool> {
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                return Ok(true);
            }
            self.done_bytes += n as u64;
            self.emit(name, false);
        }
    }

    /// 逐条目解压 TAR / TAR.GZ：以压缩输入偏移作进度（解压后总大小不可预知），
    /// entry.unpack_in 自带路径穿越防护。
    fn extract_tar(&mut self, archive: &Path, dst_dir: &Path, gzip: bool) -> io::Result<bool> {
        fs::create_dir_all(dst_dir)?;
        let file = fs::File::open(archive)?;
        let read_pos = Rc::new(Cell::new(0u64));
        let counted = CountingReader {
            inner: file,
            count: read_pos.clone(),
        };
        let reader: Box<dyn Read> = if gzip {
            Box::new(flate2::read::GzDecoder::new(counted))
        } else {
            Box::new(counted)
        };
        let mut tar = tar::Archive::new(reader);
        for entry in tar.entries()? {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                // 目录 / 链接等特殊条目：交给 unpack_in（自带路径穿越防护），不做冲突询问
                entry.unpack_in(dst_dir)?;
                self.done_bytes = read_pos.get();
                continue;
            }
            // 普通文件：路径手工清洗（unpack 本身不校验），支持冲突询问与改名
            let rel = match entry.path().ok().and_then(|p| sanitize_rel_path(&p)) {
                Some(p) => p,
                None => continue, // 未读数据由迭代器自动跳过
            };
            let outpath = dst_dir.join(&rel);
            if let Some(parent) = outpath.parent() {
                fs::create_dir_all(parent)?;
            }
            let name = name_of(&outpath);
            let size = entry.header().size().unwrap_or(0);
            let (outcome, outpath) = self.decide_extract(&outpath, size);
            if matches!(outcome, ConflictOutcome::Skip) {
                self.done_files += 1;
                self.skipped += 1;
                self.done_bytes = read_pos.get();
                self.emit(&name, false);
                continue;
            }
            // 手工分块写入（不用 entry.unpack 的一次性内部拷贝）：
            // 逐块响应暂停/取消并上报进度，与 zip/7z 路径行为一致。
            // 进度按压缩流读取偏移推进（done_bytes 覆盖式更新，而非累加；
            // 因此解压任务入队时逐归档拆分，一个 Job 只含一个 tar 归档）
            let mut out = fs::File::create(&outpath)?;
            let mut buf = vec![0u8; 1024 * 1024];
            loop {
                self.ctrl.wait_if_paused();
                if self.ctrl.is_cancelled() {
                    drop(out);
                    let _ = fs::remove_file(&outpath); // 清理半成品
                    return Ok(false);
                }
                let n = entry.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                self.done_bytes = read_pos.get();
                self.emit(&name, false);
            }
            self.done_files += 1;
            self.done_bytes = read_pos.get();
            self.emit(&name, true);
        }
        // 收尾：把余量（尾部填充块）计入，避免进度停在 99%
        self.done_bytes = self.total_bytes.max(read_pos.get());
        self.emit("完成", true);
        Ok(true)
    }

    /// 把归档条目流分块写入目标文件：逐块响应暂停/取消并上报进度。
    /// 返回 Ok(false) 表示中途被取消（半成品已清理）。
    fn write_stream(&mut self, reader: &mut dyn Read, outpath: &Path) -> io::Result<bool> {
        let name = name_of(outpath);
        let mut out = fs::File::create(outpath)?;
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                drop(out);
                let _ = fs::remove_file(outpath); // 清理半成品
                return Ok(false);
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            self.done_bytes += n as u64;
            self.emit(&name, false);
        }
        self.done_files += 1;
        self.emit(&name, true);
        Ok(true)
    }

    // ──────────────────────── 压缩 ────────────────────────

    /// 压缩任务主流程：把 job.srcs 打包为 job.dst 指定的归档
    /// （格式按 dst 扩展名识别，入队时已避让重名）。取消时删除半成品归档。
    fn run_compress(&mut self, job: &Job) -> TaskResult {
        let fmt = match is_archive(&job.dst) {
            Some(f) => f,
            None => return self.result(false, "压缩失败：无法识别输出格式".to_string()),
        };
        let res = match fmt {
            ArchiveFormat::Zip => self.compress_zip(&job.srcs, &job.dst),
            ArchiveFormat::SevenZ => self.compress_7z(&job.srcs, &job.dst),
            ArchiveFormat::Tar => self.compress_tar(&job.srcs, &job.dst, false),
            ArchiveFormat::TarGz => self.compress_tar(&job.srcs, &job.dst, true),
        };
        match res {
            Ok(true) => {
                self.emit("完成", true);
                TaskResult {
                    ok: self.done_files as i32,
                    skipped: 0,
                    error: String::new(),
                    cancelled: false,
                    completed_paths: vec![job.dst.clone()],
                }
            }
            Ok(false) => {
                let _ = fs::remove_file(&job.dst);
                self.result(true, String::new())
            }
            Err(e) => {
                let _ = fs::remove_file(&job.dst);
                self.result(false, format!("压缩失败：{}", e))
            }
        }
    }

    /// 逐文件写入 ZIP：分块读源文件，逐块响应暂停/取消并上报进度。
    fn compress_zip(&mut self, srcs: &[PathBuf], target: &Path) -> io::Result<bool> {
        use zip::write::SimpleFileOptions;
        let file = fs::File::create(target)?;
        let mut zip = zip::ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let to_io = |e: zip::result::ZipError| io::Error::new(io::ErrorKind::Other, e.to_string());

        for item in walk_pack(srcs) {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            match item {
                PackItem::Dir(_, rel) => {
                    zip.add_directory(format!("{}/", rel), options)
                        .map_err(to_io)?;
                }
                PackItem::File(path, rel) => {
                    // 大文件（>4GiB）需在写入前声明以启用 zip64
                    let large = fs::metadata(&path).map(|m| m.len()).unwrap_or(0) >= 0xFFFF_FFFFu64;
                    zip.start_file(&rel, options.large_file(large))
                        .map_err(to_io)?;
                    let name = name_of(&path);
                    let mut reader = fs::File::open(&path)?;
                    let mut buf = vec![0u8; 1024 * 1024];
                    loop {
                        self.ctrl.wait_if_paused();
                        if self.ctrl.is_cancelled() {
                            return Ok(false);
                        }
                        let n = reader.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        zip.write_all(&buf[..n])?;
                        self.done_bytes += n as u64;
                        self.emit(&name, false);
                    }
                    self.done_files += 1;
                    self.emit(&name, true);
                }
            }
        }
        zip.finish().map_err(to_io)?;
        Ok(true)
    }

    /// 逐条目写入 7z：源文件流经 GatedReader 包装，读取时响应暂停/取消。
    /// （sevenz 库内部消费数据流，无法在外层逐块检查，进度按整文件推进。）
    fn compress_7z(&mut self, srcs: &[PathBuf], target: &Path) -> io::Result<bool> {
        let mut sz = sevenz_rust::SevenZWriter::create(target)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("7z：{}", e)))?;
        for item in walk_pack(srcs) {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            let (path, rel, is_dir) = match &item {
                PackItem::Dir(p, r) => (p.clone(), r.clone(), true),
                PackItem::File(p, r) => (p.clone(), r.clone(), false),
            };
            let entry = sevenz_rust::SevenZWriter::<fs::File>::create_archive_entry(&path, rel);
            let size = if is_dir {
                0
            } else {
                fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            };
            let reader = if is_dir {
                None
            } else {
                Some(GatedReader {
                    inner: fs::File::open(&path)?,
                    ctrl: self.ctrl,
                })
            };
            match sz.push_archive_entry(entry, reader) {
                Ok(_) => {}
                Err(sevenz_rust::Error::Io(e, _)) => {
                    // GatedReader 在取消时以 Io 错误中断压缩
                    if self.ctrl.is_cancelled() {
                        return Ok(false);
                    }
                    return Err(e);
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::Other, format!("7z：{}", e))),
            }
            if !is_dir {
                self.done_files += 1;
                self.done_bytes += size;
                self.emit(&name_of(&path), true);
            }
        }
        sz.finish()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("7z：{}", e)))?;
        Ok(true)
    }

    /// 逐条目写入 TAR / TAR.GZ：同样经 GatedReader 响应暂停/取消，
    /// 进度按整文件推进（tar 库内部消费数据流）。
    /// 用具体 writer 类型显式收尾：gzip 尾部（CRC/长度 trailer）必须经
    /// GzEncoder::finish 写出并传播错误——留给 Drop 会静默吞掉收尾 IO 错误。
    fn compress_tar(&mut self, srcs: &[PathBuf], target: &Path, gzip: bool) -> io::Result<bool> {
        let file = fs::File::create(target)?;
        if gzip {
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            if !self.pack_tar_entries(&mut builder, srcs)? {
                return Ok(false);
            }
            // into_inner 写出 tar 终止块并取回编码器；finish 冲刷剩余数据与 gzip 尾部
            builder.into_inner()?.finish()?;
        } else {
            let mut builder = tar::Builder::new(file);
            if !self.pack_tar_entries(&mut builder, srcs)? {
                return Ok(false);
            }
            builder.into_inner()?;
        }
        Ok(true)
    }

    /// 把待压缩清单逐条目写入 tar Builder。返回 Ok(false) 表示中途被取消。
    fn pack_tar_entries<W: Write>(
        &mut self,
        builder: &mut tar::Builder<W>,
        srcs: &[PathBuf],
    ) -> io::Result<bool> {
        for item in walk_pack(srcs) {
            self.ctrl.wait_if_paused();
            if self.ctrl.is_cancelled() {
                return Ok(false);
            }
            match item {
                PackItem::Dir(path, rel) => {
                    builder.append_dir(format!("{}/", rel), &path)?;
                }
                PackItem::File(path, rel) => {
                    let meta = fs::metadata(&path)?;
                    let mut header = tar::Header::new_gnu();
                    header.set_metadata(&meta);
                    let gated = GatedReader {
                        inner: fs::File::open(&path)?,
                        ctrl: self.ctrl,
                    };
                    match builder.append_data(&mut header, &rel, gated) {
                        Ok(_) => {}
                        Err(e) => {
                            if self.ctrl.is_cancelled() {
                                return Ok(false);
                            }
                            return Err(e);
                        }
                    }
                    self.done_files += 1;
                    self.done_bytes += meta.len();
                    self.emit(&name_of(&path), true);
                }
            }
        }
        Ok(true)
    }
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 组装一帧进度（比例 / 速度 / 剩余时间）。本地任务与便携设备任务共用。
#[allow(clippy::too_many_arguments)]
fn make_progress(
    op: &str,
    current: &str,
    target: &str,
    done_files: i32,
    total_files: i32,
    done_bytes: u64,
    total_bytes: u64,
    start: Instant,
) -> Progress {
    let fraction = if total_bytes == 0 {
        if total_files <= 0 {
            // 0 = 无内容；-1 = 总数未知（tar 解压），均无法给出比例
            0.0
        } else {
            done_files as f32 / total_files as f32
        }
    } else {
        (done_bytes as f64 / total_bytes as f64) as f32
    };

    let elapsed = start.elapsed().as_secs_f64();
    let (speed, eta) = if elapsed > 0.3 && done_bytes > 0 {
        let bps = done_bytes as f64 / elapsed;
        let remain = total_bytes.saturating_sub(done_bytes);
        let eta_secs = if bps > 0.0 { (remain as f64 / bps) as i64 } else { 0 };
        (format!("{}/s", human_size(bps as u64)), fmt_eta(eta_secs))
    } else {
        ("计算中…".to_string(), "计算中…".to_string())
    };

    Progress {
        operation: op.to_string(),
        current_file: current.to_string(),
        target: target.to_string(),
        completed: done_files,
        total: total_files,
        fraction: fraction.clamp(0.0, 1.0),
        speed,
        eta,
    }
}

// ══════════════════════════════════════════════════════════════════════
//  便携设备（MTP/WPD）传输
//
//  设备路径不是文件系统路径，std::fs 全线不可用，因此 Copy/Move 只要有一端
//  落在 device:// 上就整体改走本节：设备↔电脑用 WPD 流传输，设备内部优先用
//  设备自带的 Move 命令（无需搬数据），不支持时回退「拉到临时目录再推回去」。
// ══════════════════════════════════════════════════════════════════════

/// 路径是否指向便携设备
pub fn is_device(p: &Path) -> bool {
    p.to_string_lossy().starts_with("device://")
}

/// 任务是否涉及便携设备（任一端在设备上）
fn job_touches_device(job: &Job) -> bool {
    is_device(&job.dst) || job.srcs.iter().any(|p| is_device(p))
}

/// 设备任务的进度状态，同时充当 devices::Sink 接收底层传输回调
struct MtpRun<'a, F: Fn(Progress)> {
    ctrl: &'a TaskControl,
    report: &'a F,
    op: &'static str,
    target: String,
    total_files: i32,
    total_bytes: u64,
    done_files: i32,
    done_bytes: u64,
    current: String,
    start: Instant,
    last_emit: Instant,
}

impl<'a, F: Fn(Progress)> MtpRun<'a, F> {
    fn emit(&mut self, force: bool) {
        if !force && self.last_emit.elapsed() < Duration::from_millis(40) {
            return;
        }
        self.last_emit = Instant::now();
        (self.report)(make_progress(
            self.op,
            &self.current,
            &self.target,
            self.done_files,
            self.total_files,
            self.done_bytes,
            self.total_bytes,
            self.start,
        ));
    }
}

impl<'a, F: Fn(Progress)> super::devices::Sink for MtpRun<'a, F> {
    fn begin_file(&mut self, name: &str, _size: u64) {
        self.current = name.to_string();
        self.emit(true);
    }

    fn advance(&mut self, bytes: u64) -> bool {
        self.done_bytes += bytes;
        self.ctrl.wait_if_paused();
        if self.ctrl.is_cancelled() {
            return false;
        }
        self.emit(false);
        true
    }

    fn end_file(&mut self) {
        self.done_files += 1;
        self.emit(true);
    }
}

/// 在设备目录下为 `name` 找一个未被占用的名称（「文件 (2).txt」）
fn free_name_on_device(parent_vpath: &str, name: &str) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        // 前导点的隐藏文件（".gitignore"）整体视为名称，不拆扩展名
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{}", e)),
        _ => (name.to_string(), String::new()),
    };
    for n in 2..1000 {
        let candidate = format!("{} ({}){}", stem, n, ext);
        if super::devices::child_named(parent_vpath, &candidate).is_none() {
            return candidate;
        }
    }
    format!("{} (副本){}", stem, ext)
}

/// 顶层同名冲突的处置结果
enum DevPlan {
    /// 跳过该项
    Skip,
    /// 继续写入；Some(名称) 表示改名写入（「保留两者」）
    Write(Option<String>),
}

/// 执行涉及便携设备的复制 / 移动任务
fn run_mtp(
    job: Job,
    ctrl: Arc<TaskControl>,
    report: impl Fn(Progress),
    ask: impl Fn(ConflictQuery) -> ConflictReply,
) -> TaskResult {
    use super::devices;

    // 设备访问要求本线程已初始化 COM；任务线程是裸 spawn 出来的，须自行初始化。
    // 重复调用返回 S_FALSE，忽略即可。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Com::{
            CoInitializeEx, COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED,
        };
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    }

    let op = job.kind.label();
    let is_move = job.kind == TaskKind::Move;
    let dst_s = job.dst.to_string_lossy().to_string();
    let dst_is_dev = devices::is_device_path(&dst_s);
    let target = if dst_is_dev {
        super::virtualfs::friendly_title(&dst_s)
    } else {
        dst_s.clone()
    };

    let mut run = MtpRun {
        ctrl: &ctrl,
        report: &report,
        op,
        target,
        total_files: 0,
        total_bytes: 0,
        done_files: 0,
        done_bytes: 0,
        current: String::new(),
        start: Instant::now(),
        last_emit: Instant::now() - Duration::from_secs(1),
    };
    run.current = "统计中…".to_string();
    run.emit(true);

    // 总量统计：设备侧要逐层枚举（慢），本地侧沿用普通扫描
    for src in &job.srcs {
        if ctrl.is_cancelled() {
            return TaskResult {
                ok: 0,
                skipped: 0,
                error: String::new(),
                cancelled: true,
                completed_paths: Vec::new(),
            };
        }
        if is_device(src) {
            let (f, b) = devices::tree_totals(&src.to_string_lossy());
            run.total_files += f;
            run.total_bytes += b;
        } else {
            let (f, b) = scan(std::slice::from_ref(src));
            run.total_files += f;
            run.total_bytes += b;
        }
    }
    run.current = "准备中…".to_string();
    run.emit(true);

    let mut ok = 0;
    let mut skipped = 0;
    let mut error = String::new();
    let mut remembered: Option<ConflictDecision> = None;

    for src in &job.srcs {
        if ctrl.is_cancelled() {
            return TaskResult {
                ok,
                skipped,
                error,
                cancelled: true,
                completed_paths: Vec::new(),
            };
        }
        let src_s = src.to_string_lossy().to_string();
        let src_is_dev = devices::is_device_path(&src_s);

        // 源项名称：设备侧读对象属性，本地侧取文件名
        let name = if src_is_dev {
            devices::object_info(&src_s)
                .map(|(n, _, _)| n)
                .unwrap_or_default()
        } else {
            name_of(src)
        };
        if name.trim().is_empty() {
            continue;
        }

        let plan = plan_for(
            &name,
            src,
            src_is_dev,
            &job.dst,
            dst_is_dev,
            op,
            &mut remembered,
            &ask,
        );
        let as_name = match plan {
            DevPlan::Skip => {
                skipped += 1;
                // 跳过项计入进度，避免比例卡在不到 100%
                if src_is_dev {
                    let (f, b) = devices::tree_totals(&src_s);
                    run.done_files += f;
                    run.done_bytes += b;
                } else {
                    let (f, b) = scan(std::slice::from_ref(src));
                    run.done_files += f;
                    run.done_bytes += b;
                }
                run.emit(true);
                continue;
            }
            DevPlan::Write(n) => n,
        };

        let res = transfer_one(
            &src_s, src, src_is_dev, &job.dst, &dst_s, dst_is_dev, as_name, is_move, &mut run,
        );
        match res {
            Ok(()) => ok += 1,
            Err(e) if e == "已取消" => {
                return TaskResult {
                    ok,
                    skipped,
                    error,
                    cancelled: true,
                    completed_paths: Vec::new(),
                }
            }
            Err(e) => error = e,
        }
    }

    run.current.clear();
    run.emit(true);
    TaskResult {
        ok,
        skipped,
        error,
        cancelled: false,
        completed_paths: Vec::new(),
    }
}

/// 顶层同名冲突：查目标端是否已有同名项，有则询问用户并执行前置动作
#[allow(clippy::too_many_arguments)]
fn plan_for(
    name: &str,
    src: &Path,
    src_is_dev: bool,
    dst: &Path,
    dst_is_dev: bool,
    op: &str,
    remembered: &mut Option<ConflictDecision>,
    ask: &impl Fn(ConflictQuery) -> ConflictReply,
) -> DevPlan {
    use super::devices;

    let dst_s = dst.to_string_lossy().to_string();
    // 目标端已存在的同名项：(设备虚拟路径或本地路径, 是否目录, 大小)
    let existing: Option<(String, bool, u64)> = if dst_is_dev {
        devices::child_named(&dst_s, name)
    } else {
        let p = dst.join(name);
        if p.exists() {
            let meta = std::fs::metadata(&p).ok();
            Some((
                p.to_string_lossy().to_string(),
                meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                meta.map(|m| m.len()).unwrap_or(0),
            ))
        } else {
            None
        }
    };
    let Some((existing_path, existing_is_dir, existing_size)) = existing else {
        return DevPlan::Write(None);
    };

    let decision = match *remembered {
        Some(d) => d,
        None => {
            // 源信息：设备端没有文件系统元数据，只给大小
            let src_info = if src_is_dev {
                match devices::object_info(&src.to_string_lossy()) {
                    Some((_, true, _)) => "文件夹 · 便携设备".to_string(),
                    Some((_, false, sz)) => format!("{} · 便携设备", human_size(sz)),
                    None => "便携设备".to_string(),
                }
            } else {
                describe(src)
            };
            let dst_info = if dst_is_dev {
                if existing_is_dir {
                    "文件夹 · 便携设备".to_string()
                } else {
                    format!("{} · 便携设备", human_size(existing_size))
                }
            } else {
                describe(Path::new(&existing_path))
            };
            let reply = ask(ConflictQuery {
                name: name.to_string(),
                operation: op.to_string(),
                src_info,
                dst_info,
                is_dir: existing_is_dir,
            });
            if reply.apply_all {
                *remembered = Some(reply.decision);
            }
            reply.decision
        }
    };

    match decision {
        ConflictDecision::Skip => DevPlan::Skip,
        ConflictDecision::Rename => DevPlan::Write(Some(if dst_is_dev {
            free_name_on_device(&dst_s, name)
        } else {
            name_of(&resolve_conflict(dst.join(name)))
        })),
        ConflictDecision::Overwrite => {
            if dst_is_dev {
                let _ = devices::delete(&[existing_path]);
            } else {
                let p = Path::new(&existing_path);
                if existing_is_dir {
                    let _ = fs::remove_dir_all(p);
                } else {
                    let _ = fs::remove_file(p);
                }
            }
            DevPlan::Write(None)
        }
    }
}

/// 搬运单个顶层项。四种方向：设备→本地、本地→设备、同设备内、跨设备。
#[allow(clippy::too_many_arguments)]
fn transfer_one<F: Fn(Progress)>(
    src_s: &str,
    src: &Path,
    src_is_dev: bool,
    dst: &Path,
    dst_s: &str,
    dst_is_dev: bool,
    as_name: Option<String>,
    is_move: bool,
    run: &mut MtpRun<F>,
) -> Result<(), String> {
    use super::devices;
    let as_name_ref = as_name.as_deref();

    match (src_is_dev, dst_is_dev) {
        // 设备 → 电脑
        (true, false) => {
            devices::pull_tree(src_s, dst, as_name_ref, run)?;
            if is_move {
                devices::delete(&[src_s.to_string()])?;
            }
        }
        // 电脑 → 设备
        (false, true) => {
            devices::push_tree(src, dst_s, as_name_ref, run)?;
            if is_move {
                if src.is_dir() {
                    fs::remove_dir_all(src).map_err(|e| format!("删除源目录失败：{}", e))?;
                } else {
                    fs::remove_file(src).map_err(|e| format!("删除源文件失败：{}", e))?;
                }
            }
        }
        // 设备 → 设备
        (true, true) => {
            let same_device = devices::device_id_of(src_s) == devices::device_id_of(dst_s);
            // 同设备且不需要改名：优先让设备自己搬（不经过 USB 传数据，秒完成）
            if same_device && is_move && as_name_ref.is_none() {
                // 必须在移动前统计：move_objects 成功后源对象已不存在，tree_totals 会返回 0
                let (f, b) = devices::tree_totals(src_s);
                if devices::move_objects(&[src_s.to_string()], dst_s).is_ok() {
                    run.done_files += f;
                    run.done_bytes += b;
                    run.emit(true);
                    return Ok(());
                }
                // 设备不支持 Move 命令：落到下面的中转路径
            }
            // 中转：先拉到临时目录，再推到目标设备目录。
            // 暂存目录带进程 ID，避免与其它会话的中转冲突
            let staging = std::env::temp_dir().join(format!(
                "FileFiles One_mtp_xfer_{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging).map_err(|e| format!("创建暂存目录失败：{}", e))?;
            let result = (|| -> Result<(), String> {
                devices::pull_tree(src_s, &staging, None, run)?;
                let staged = fs::read_dir(&staging)
                    .map_err(|e| format!("读取暂存目录失败：{}", e))?
                    .flatten()
                    .next()
                    .ok_or("暂存目录为空")?
                    .path();
                // 推送阶段的字节已在拉取阶段计入，这里不重复累加进度
                devices::push_tree(&staged, dst_s, as_name_ref, &mut NoProgress)?;
                if is_move {
                    devices::delete(&[src_s.to_string()])?;
                }
                Ok(())
            })();
            let _ = fs::remove_dir_all(&staging);
            result?;
        }
        // 两端都不在设备上：不会走到这里（run 只在涉及设备时分流）
        (false, false) => return Err("非设备传输".to_string()),
    }
    Ok(())
}

/// 中转推送阶段用的空进度接收器（字节已在拉取阶段计过一次）
struct NoProgress;
impl super::devices::Sink for NoProgress {
    fn begin_file(&mut self, _name: &str, _size: u64) {}
    fn advance(&mut self, _bytes: u64) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    // 创建隔离的临时测试目录
    fn temp_dir(tag: &str) -> PathBuf {
        let mut d = env::temp_dir();
        let unique = format!(
            "filefiles_task_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        d.push(unique);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// 直接驱动 run()：无进度消费者，冲突固定回复
    fn run_job(job: Job, decision: ConflictDecision) -> TaskResult {
        run(
            job,
            Arc::new(TaskControl::new()),
            |_| {},
            move |_| ConflictReply {
                decision,
                apply_all: true,
            },
        )
    }

    /// 构造源目录：一个顶层文件 + 一个含子文件的文件夹
    fn make_source(dir: &Path) -> PathBuf {
        let src = dir.join("源目录");
        fs::create_dir_all(src.join("子")).unwrap();
        fs::write(src.join("顶层.txt"), b"hello").unwrap();
        fs::write(src.join("子").join("嵌套.txt"), b"world").unwrap();
        src
    }

    /// 校验解压结果的结构与内容
    fn assert_roundtrip(out: &Path) {
        assert_eq!(
            fs::read(out.join("源目录").join("顶层.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            fs::read(out.join("源目录").join("子").join("嵌套.txt")).unwrap(),
            b"world"
        );
    }

    /// 对指定扩展名做「后台任务压缩 → 后台任务解压 → 内容校验」全流程
    fn roundtrip(ext: &str) {
        let dir = temp_dir(ext.split('.').next_back().unwrap());
        let src = make_source(&dir);

        let target = dir.join(format!("归档.{}", ext));
        let res = run_job(
            Job {
                kind: TaskKind::Compress,
                srcs: vec![src.clone()],
                dst: target.clone(),
            },
            ConflictDecision::Skip,
        );
        assert!(res.error.is_empty(), "压缩失败: {}", res.error);
        assert!(target.exists());
        assert_eq!(res.ok, 2); // 两个文件

        let out = dir.join("解压结果");
        let res = run_job(
            Job {
                kind: TaskKind::Extract,
                srcs: vec![target],
                dst: out.clone(),
            },
            ConflictDecision::Skip,
        );
        assert!(res.error.is_empty(), "解压失败: {}", res.error);
        assert_eq!(res.ok, 2);
        assert_roundtrip(&out);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_roundtrip_zip() {
        roundtrip("zip");
    }

    #[test]
    fn test_roundtrip_7z() {
        roundtrip("7z");
    }

    #[test]
    fn test_roundtrip_tar() {
        roundtrip("tar");
    }

    #[test]
    fn test_roundtrip_tar_gz() {
        roundtrip("tar.gz");
    }

    #[test]
    fn test_extract_conflict_skip_and_overwrite() {
        let dir = temp_dir("conflict");
        let src = make_source(&dir);
        let target = dir.join("归档.zip");
        run_job(
            Job {
                kind: TaskKind::Compress,
                srcs: vec![src],
                dst: target.clone(),
            },
            ConflictDecision::Skip,
        );

        // 预置一个同名旧文件，Skip 决策应保留其内容
        let out = dir.join("解压结果");
        fs::create_dir_all(out.join("源目录")).unwrap();
        fs::write(out.join("源目录").join("顶层.txt"), b"old").unwrap();
        let res = run_job(
            Job {
                kind: TaskKind::Extract,
                srcs: vec![target.clone()],
                dst: out.clone(),
            },
            ConflictDecision::Skip,
        );
        assert_eq!(res.skipped, 1);
        assert_eq!(
            fs::read(out.join("源目录").join("顶层.txt")).unwrap(),
            b"old"
        );

        // Overwrite 决策应覆盖为归档内容
        let res = run_job(
            Job {
                kind: TaskKind::Extract,
                srcs: vec![target],
                dst: out.clone(),
            },
            ConflictDecision::Overwrite,
        );
        assert_eq!(res.skipped, 0);
        assert!(res.error.is_empty());
        assert_eq!(
            fs::read(out.join("源目录").join("顶层.txt")).unwrap(),
            b"hello"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_sanitize_rel_path_rejects_traversal() {
        assert_eq!(sanitize_rel_path(Path::new("..\\逃逸.txt")), None);
        assert_eq!(sanitize_rel_path(Path::new("../逃逸.txt")), None);
        assert_eq!(sanitize_rel_path(Path::new("C:\\Windows\\a.dll")), None);
        assert_eq!(sanitize_rel_path(Path::new("/etc/passwd")), None);
        assert_eq!(sanitize_rel_path(Path::new("")), None);
        assert_eq!(
            sanitize_rel_path(Path::new("./a/b.txt")),
            Some(PathBuf::from("a").join("b.txt"))
        );
    }

    #[test]
    fn test_cancelled_before_start() {
        let dir = temp_dir("cancel");
        let src = make_source(&dir);
        let ctrl = Arc::new(TaskControl::new());
        ctrl.cancel();
        let res = run(
            Job {
                kind: TaskKind::Compress,
                srcs: vec![src],
                dst: dir.join("归档.zip"),
            },
            ctrl,
            |_| {},
            |_| ConflictReply {
                decision: ConflictDecision::Skip,
                apply_all: false,
            },
        );
        assert!(res.cancelled);
        // 半成品归档应被清理
        assert!(!dir.join("归档.zip").exists());
        fs::remove_dir_all(&dir).ok();
    }
}

/// 把剩余秒数格式化为「X 分 Y 秒」/「Y 秒」
fn fmt_eta(secs: i64) -> String {
    if secs <= 0 {
        return "即将完成".to_string();
    }
    if secs < 60 {
        format!("{} 秒", secs)
    } else {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    }
}
