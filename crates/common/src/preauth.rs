//! 認証前 (Noise ハンドシェイク中) 接続の枠管理。host と WoL 中継で共有する。
//!
//! 全体枠だけだと、単一の未認証ピアが無言接続やバックオフ待ちで枠を埋め、正規クライアントを
//! 締め出せる。Tailnet では 1 ノード = 1 IP なので、送信元 IP 単位の同時数上限を重ねて
//! 「1 ノードでは全体枠を食い潰せない」ようにする。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 認証前接続の枠を払い出す。clone は同じ枠を共有する。
#[derive(Clone)]
pub struct PreauthLimiter {
    total: Arc<Semaphore>,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
    per_ip_max: usize,
}

/// 払い出した 1 枠。drop で全体枠と IP 別カウントの両方を返す。認証の成否が決まった時点
/// (失敗時のバックオフ待ちに入る前) で drop し、待ち時間に枠を握らせないこと。
pub struct PreauthSlot {
    _permit: OwnedSemaphorePermit,
    ip: IpAddr,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

/// 枠を払い出せなかった理由 (ログ用)。
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum PreauthReject {
    /// 全体枠が埋まっている。
    #[error("pre-auth slots exhausted")]
    Total,
    /// この送信元 IP の同時数が上限に達している。
    #[error("pre-auth slots for this source IP exhausted")]
    PerIp,
}

impl PreauthLimiter {
    /// 全体 `total` 枠、送信元 IP あたり `per_ip_max` 枠の制限器を作る。
    pub fn new(total: usize, per_ip_max: usize) -> Self {
        Self {
            total: Arc::new(Semaphore::new(total)),
            per_ip: Arc::new(Mutex::new(HashMap::new())),
            per_ip_max,
        }
    }

    /// `ip` からの接続に 1 枠を払い出す。待たずに即判定する (accept ループを止めないため)。
    pub fn try_acquire(&self, ip: IpAddr) -> Result<PreauthSlot, PreauthReject> {
        // poison は保持中の panic でしか起きず、カウント自体は整合しているので中身を使い続ける。
        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        let count = map.get(&ip).copied().unwrap_or(0);
        if count >= self.per_ip_max {
            return Err(PreauthReject::PerIp);
        }
        let permit = self
            .total
            .clone()
            .try_acquire_owned()
            .map_err(|_| PreauthReject::Total)?;
        map.insert(ip, count + 1);
        Ok(PreauthSlot {
            _permit: permit,
            ip,
            per_ip: self.per_ip.clone(),
        })
    }
}

impl Drop for PreauthSlot {
    fn drop(&mut self) {
        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        // 0 になった IP は消し、送信元が入れ替わり続けてもマップが増え続けないようにする。
        if let Some(n) = map.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([100, 64, 0, last])
    }

    #[test]
    fn per_ip_cap_does_not_block_other_ips() {
        let lim = PreauthLimiter::new(8, 2);
        let _a1 = lim.try_acquire(ip(1)).unwrap();
        let _a2 = lim.try_acquire(ip(1)).unwrap();
        assert_eq!(lim.try_acquire(ip(1)).err(), Some(PreauthReject::PerIp));
        assert!(lim.try_acquire(ip(2)).is_ok());
    }

    #[test]
    fn total_cap_applies_across_ips() {
        let lim = PreauthLimiter::new(2, 2);
        let _a = lim.try_acquire(ip(1)).unwrap();
        let _b = lim.try_acquire(ip(2)).unwrap();
        assert_eq!(lim.try_acquire(ip(3)).err(), Some(PreauthReject::Total));
    }

    #[test]
    fn total_reject_does_not_leak_per_ip_count() {
        let lim = PreauthLimiter::new(1, 1);
        let held = lim.try_acquire(ip(1)).unwrap();
        assert_eq!(lim.try_acquire(ip(2)).err(), Some(PreauthReject::Total));
        drop(held);
        // ip(2) の拒否でカウントが残っていれば PerIp になる。
        assert!(lim.try_acquire(ip(2)).is_ok());
    }

    #[test]
    fn drop_releases_both_caps() {
        let lim = PreauthLimiter::new(1, 1);
        let s = lim.try_acquire(ip(1)).unwrap();
        drop(s);
        assert!(lim.try_acquire(ip(1)).is_ok());
        assert!(lim.per_ip.lock().unwrap().len() <= 1);
    }
}
