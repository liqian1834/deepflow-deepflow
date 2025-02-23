/*
 * Copyright (c) 2022 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::HashMap, os::unix::process::CommandExt, process::Command};

use envmnt::{ExpandOptions, ExpansionType};

use log::error;
use nom::AsBytes;
use procfs::{process::Process, ProcError, ProcResult};
use public::bytes::write_u64_be;
use public::proto::trident::{ProcessInfo, Tag};
use public::pwd::PasswordInfo;
use regex::Regex;
use ring::digest;
use serde::Deserialize;

use super::proc_scan_hook::proc_scan_hook;
use super::{dir_inode, SHA1_DIGEST_LEN};

use crate::config::handler::OsProcScanConfig;
use crate::config::{
    OsProcRegexp, OS_PROC_REGEXP_MATCH_ACTION_ACCEPT, OS_PROC_REGEXP_MATCH_ACTION_DROP,
    OS_PROC_REGEXP_MATCH_TYPE_CMD, OS_PROC_REGEXP_MATCH_TYPE_PROC_NAME,
};
#[derive(Debug, Clone)]
pub struct ProcessData {
    pub name: String, // the replaced name
    pub pid: u64,
    pub process_name: String, // raw process name
    pub cmd: Vec<String>,
    pub user_id: u32,
    pub user: String,
    pub start_time: Duration, // the process start timestamp
    // Vec<key, val>
    pub os_app_tags: Vec<OsAppTagKV>,
}

impl ProcessData {
    // proc data only hash the pid and tag
    pub fn digest(&self, dist_ctx: &mut digest::Context) {
        let mut pid = [0u8; 8];
        write_u64_be(&mut pid, self.pid);

        dist_ctx.update(&pid);

        for i in self.os_app_tags.iter() {
            dist_ctx.update(i.key.as_bytes());
            dist_ctx.update(i.value.as_bytes());
        }
    }

    pub(super) fn up_sec(&self, base_time: u64) -> Result<u64, ProcError> {
        let start_time_sec = self.start_time.as_secs();
        if base_time < self.start_time.as_secs() {
            Err(ProcError::Other("proc start time gt base time".into()))
        } else {
            Ok(base_time - start_time_sec)
        }
    }

    // get the inode of /proc/pid/root
    pub(super) fn get_root_inode(&mut self, proc_root: &str) -> std::io::Result<u64> {
        // /proc/{pid}/root
        let p = PathBuf::from_iter([proc_root, self.pid.to_string().as_str(), "root"]);
        dir_inode(p.to_str().unwrap())
    }

    pub(super) fn set_username(&mut self, pwd: &PasswordInfo) {
        if let Some(u) = pwd.get_username_by_uid(self.user_id) {
            self.user = u;
        }
    }
}

// need sort by pid before calc the hash
pub fn calc_process_datas_sha1(data: &Vec<ProcessData>) -> [u8; SHA1_DIGEST_LEN] {
    let mut h = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);

    for i in data {
        i.digest(&mut h)
    }

    let mut ret = [0u8; SHA1_DIGEST_LEN];
    ret.copy_from_slice(h.finish().as_ref().as_bytes());
    ret
}

impl TryFrom<&Process> for ProcessData {
    type Error = ProcError;
    // will not set the username
    fn try_from(proc: &Process) -> Result<Self, Self::Error> {
        let (exe, cmd, uid) = (proc.exe()?, proc.cmdline()?, proc.uid()?);
        let Some(proc_name) = exe.file_name() else {
            return Err(ProcError::Other(format!("pid {} get process name fail", proc.pid).to_string()));
        };

        Ok(ProcessData {
            name: proc_name.to_string_lossy().to_string(),
            pid: proc.pid as u64,
            process_name: proc_name.to_string_lossy().to_string(),
            cmd,
            user_id: uid,
            user: "".to_string(),
            start_time: {
                if let Ok(stat) = proc.stat() {
                    let z = stat.starttime().unwrap_or_default();
                    Duration::from_secs(z.timestamp() as u64)
                } else {
                    Duration::ZERO
                }
            },
            os_app_tags: vec![],
        })
    }
}

// convert ProcessData to ProcessInfo pb struct
impl From<&ProcessData> for ProcessInfo {
    fn from(p: &ProcessData) -> Self {
        Self {
            name: Some(p.name.clone()),
            pid: Some(p.pid),
            process_name: Some(p.process_name.clone()),
            cmdline: Some(p.cmd.join(" ")),
            user: Some(p.user.clone()),
            start_time: Some(u32::try_from(p.start_time.as_secs()).unwrap_or_default()),
            os_app_tags: {
                let mut tags = vec![];
                for t in p.os_app_tags.iter() {
                    tags.push(Tag {
                        key: Some(t.key.clone()),
                        value: Some(t.value.clone()),
                    })
                }
                tags
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum RegExpAction {
    Accept,
    Drop,
}

impl Default for RegExpAction {
    fn default() -> Self {
        Self::Accept
    }
}

#[derive(Clone, Debug)]
pub enum ProcRegRewrite {
    // (match reg, action, rewrite string)
    Cmd(Regex, RegExpAction, String),
    ProcessName(Regex, RegExpAction, String),
}

impl ProcRegRewrite {
    pub fn action(&self) -> RegExpAction {
        match self {
            ProcRegRewrite::Cmd(_, act, _) => *act,
            ProcRegRewrite::ProcessName(_, act, _) => *act,
        }
    }
}

impl PartialEq for ProcRegRewrite {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Cmd(lr, lact, ls), Self::Cmd(rr, ract, rs)) => {
                lr.as_str() == rr.as_str() && lact == ract && ls == rs
            }
            (Self::ProcessName(lr, lact, ls), Self::ProcessName(rr, ract, rs)) => {
                lr.as_str() == rr.as_str() && lact == ract && ls == rs
            }
            _ => false,
        }
    }
}

impl Eq for ProcRegRewrite {}

impl TryFrom<&OsProcRegexp> for ProcRegRewrite {
    type Error = regex::Error;

    fn try_from(value: &OsProcRegexp) -> Result<Self, Self::Error> {
        let re = Regex::new(value.match_regex.as_str())?;
        let action = match value.action.as_str() {
            "" | OS_PROC_REGEXP_MATCH_ACTION_ACCEPT => RegExpAction::Accept,
            OS_PROC_REGEXP_MATCH_ACTION_DROP => RegExpAction::Drop,
            _ => return Err(regex::Error::Syntax("action must accept or drop".into())),
        };
        let env_rewrite = |r: String| {
            envmnt::expand(
                r.as_str(),
                Some(ExpandOptions {
                    expansion_type: Some(ExpansionType::Windows),
                    default_to_empty: true,
                }),
            )
        };

        match value.match_type.as_str() {
            OS_PROC_REGEXP_MATCH_TYPE_CMD => Ok(Self::Cmd(
                re,
                action,
                env_rewrite(value.rewrite_name.clone()),
            )),
            "" | OS_PROC_REGEXP_MATCH_TYPE_PROC_NAME => Ok(Self::ProcessName(
                re,
                action,
                env_rewrite(value.rewrite_name.clone()),
            )),
            _ => Err(regex::Error::__Nonexhaustive),
        }
    }
}

impl ProcRegRewrite {
    pub(super) fn match_and_rewrite_proc(&self, proc: &mut ProcessData, match_only: bool) -> bool {
        let mut match_replace_fn =
            |reg: &Regex, act: &RegExpAction, s: &String, replace: &String| {
                if reg.is_match(s.as_str()) {
                    if act == &RegExpAction::Accept && !replace.is_empty() && !match_only {
                        proc.name = reg.replace_all(s.as_str(), replace).to_string();
                    }
                    true
                } else {
                    false
                }
            };

        match self {
            ProcRegRewrite::Cmd(reg, act, replace) => {
                match_replace_fn(reg, act, &proc.cmd.join(" "), replace)
            }
            ProcRegRewrite::ProcessName(reg, act, replace) => {
                match_replace_fn(reg, act, &proc.process_name, replace)
            }
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct OsAppTagKV {
    pub key: String,
    pub value: String,
}

#[derive(Default, Deserialize)]
struct OsAppTag {
    pid: u64,
    // Vec<key, val>
    tags: Vec<OsAppTagKV>,
}

pub(super) fn get_all_process(conf: &OsProcScanConfig) -> Vec<ProcessData> {
    // Hashmap<root_inode, PasswordInfo>
    let mut pwd_info = HashMap::new();
    let (user, cmd, proc_root, proc_regexp, now_sec) = (
        conf.os_app_tag_exec_user.as_str(),
        conf.os_app_tag_exec.as_slice(),
        conf.os_proc_root.as_str(),
        conf.os_proc_regex.as_slice(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );

    let mut tags_map = match get_os_app_tag_by_exec(user, cmd) {
        Ok(tags) => tags,
        Err(err) => {
            error!(
                "get process tags by execute cmd `{}` with user {} fail: {}",
                cmd.join(" "),
                user,
                err
            );
            HashMap::new()
        }
    };

    let mut ret = vec![];
    if let Ok(procs) = procfs::process::all_processes_with_root(proc_root) {
        for proc in procs {
            if let Err(err) = proc {
                error!("get process fail: {}", err);
                continue;
            }
            let Ok(mut proc_data) =  ProcessData::try_from(proc.as_ref().unwrap()) else {
                continue;
            };
            let Ok(up_sec) = proc_data.up_sec(now_sec) else {
                continue;
            };

            // filter the short live proc
            if up_sec < u64::from(conf.os_proc_socket_min_lifetime) {
                continue;
            }

            for i in proc_regexp.iter() {
                if i.match_and_rewrite_proc(&mut proc_data, false) {
                    if i.action() == RegExpAction::Drop {
                        break;
                    }

                    // get pwd info from hashmap or parse pwd file from /proc/pid/root/etc/passwd and insert to hashmap
                    // and get the username from pwd info
                    match proc_data.get_root_inode(proc_root) {
                        Err(e) => error!("pid {} get root inode fail: {}", proc_data.pid, e),
                        Ok(inode) => {
                            if let Some(pwd) = pwd_info.get(&inode) {
                                proc_data.set_username(&pwd);
                            } else {
                                // not in hashmap, parse from /proc/pid/root/etc/passwd
                                let p = PathBuf::from_iter([
                                    proc_root,
                                    proc_data.pid.to_string().as_str(),
                                    "root/etc/passwd",
                                ]);
                                if let Ok(pwd) = PasswordInfo::new(p) {
                                    proc_data.set_username(&pwd);
                                    pwd_info.insert(inode, pwd);
                                }
                            }
                        }
                    }

                    // fill tags
                    if let Some(tags) = tags_map.remove(&proc_data.pid) {
                        proc_data.os_app_tags = tags.tags
                    }

                    ret.push(proc_data);
                    break;
                }
            }
        }
        proc_scan_hook(&mut ret);
    }
    return ret;
}

pub(super) fn get_self_proc() -> ProcResult<ProcessData> {
    let proc = procfs::process::Process::myself()?;
    let mut path = proc.root()?;
    path.push("etc/passwd");
    let pwd = PasswordInfo::new(path).map_err(|e| ProcError::Other(e.to_string()))?;
    let mut proc_data = ProcessData::try_from(&proc)?;
    proc_data.set_username(&pwd);
    Ok(proc_data)
}

// return Hashmap<pid, OsAppTag>
fn get_os_app_tag_by_exec(
    username: &str,
    cmd: &[String],
) -> Result<HashMap<u64, OsAppTag>, String> {
    if username.is_empty() || cmd.len() == 0 {
        return Ok(HashMap::new());
    }

    let pwd_info = PasswordInfo::new("/etc/passwd").map_err(|e| e.to_string())?;
    let Some(uid) = pwd_info.get_uid_by_username(username) else {
        return Err(format!("get userid by username {} fail", username).to_string());
    };

    let mut exec_cmd = Command::new(&cmd[0]);
    exec_cmd.uid(uid).args(&cmd[1..]);

    let output = exec_cmd.output();
    if let Err(err) = output {
        return Err(err.to_string());
    };
    let output = output.unwrap();
    let stdout = String::from_utf8_lossy(output.stdout.as_ref()).to_string();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(output.stderr.as_ref()).to_string();
        return Err(format!(
            "exit_status: {}\nstdout: {}\nstderr: {}",
            output.status, stdout, stderr
        ));
    };

    match serde_yaml::from_str::<Vec<OsAppTag>>(stdout.as_str()) {
        Ok(tags) => Ok(HashMap::from_iter(tags.into_iter().map(|t| (t.pid, t)))),
        Err(e) => Err(format!("unmarshal to yaml fail: {}\nstdout: {}", e, stdout).to_string()),
    }
}
