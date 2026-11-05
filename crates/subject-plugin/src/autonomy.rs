use eve_subject_api::*;

/// 跳过停机期间错过的窗口，不累积额度；重启和窗口内轮询不刷新起点。
pub fn advance_autonomy_window(
    window: &mut AutonomyWindow,
    limit: u32,
    now: u64,
) -> SubjectResult<bool> {
    window.validate(limit)?;
    let elapsed = now
        .checked_sub(window.started_at_ms)
        .ok_or(SubjectError::InvalidTransition)?;
    if elapsed < window.period_ms {
        return Ok(false);
    }
    let started_at_ms = now - elapsed % window.period_ms;
    let next = AutonomyWindow {
        started_at_ms,
        period_ms: window.period_ms,
        used: 0,
    };
    next.validate(limit)?;
    *window = next;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_does_not_refresh_budget_and_missed_windows_do_not_accumulate() {
        let mut window = AutonomyWindow {
            started_at_ms: 100,
            period_ms: AUTONOMY_DAY_MS,
            used: 20,
        };
        assert!(!advance_autonomy_window(&mut window, 20, 1000).unwrap());
        assert_eq!(window.used, 20);
        assert!(advance_autonomy_window(&mut window, 20, 100 + 7 * AUTONOMY_DAY_MS + 1).unwrap());
        assert_eq!(window.started_at_ms, 100 + 7 * AUTONOMY_DAY_MS);
        assert_eq!(window.used, 0);
        window.used = 1;
        assert!(!advance_autonomy_window(&mut window, 20, 101 + 7 * AUTONOMY_DAY_MS).unwrap());
        assert_eq!(window.used, 1);
    }
    #[test]
    fn clock_reversal_and_invalid_window_preserve_old_usage() {
        let mut window = AutonomyWindow {
            started_at_ms: 100,
            period_ms: AUTONOMY_DAY_MS,
            used: 20,
        };
        let before = window.clone();
        assert!(advance_autonomy_window(&mut window, 20, 99).is_err());
        assert!(advance_autonomy_window(&mut window, 10, 100 + AUTONOMY_DAY_MS).is_err());
        assert_eq!(window, before);
    }
}
