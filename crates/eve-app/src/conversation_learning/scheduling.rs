//! 单 worker 在增量学习和历史维护之间交替领取；空队列不占用另一类的机会。
#[derive(Default)]
pub(super) struct AlternatingBatches {
    maintenance_next: bool,
}

impl AlternatingBatches {
    /// 领取成功只返回一个批次。下一次优先另一类，预算和持久在途检查仍由领取器负责。
    pub(super) fn take<T, E>(
        &mut self,
        mut normal: impl FnMut() -> Result<Option<T>, E>,
        mut maintenance: impl FnMut() -> Result<Option<T>, E>,
    ) -> Result<Option<(bool, T)>, E> {
        let selected = if self.maintenance_next {
            match maintenance()? {
                Some(batch) => Some((true, batch)),
                None => normal()?.map(|batch| (false, batch)),
            }
        } else {
            match normal()? {
                Some(batch) => Some((false, batch)),
                None => maintenance()?.map(|batch| (true, batch)),
            }
        };
        if let Some((is_maintenance, _)) = &selected {
            self.maintenance_next = !is_maintenance;
        }
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn ready_queues_alternate_without_claiming_two_batches() {
        let mut scheduler = AlternatingBatches::default();
        let calls = RefCell::new(vec![]);
        let mut selected = vec![];
        for _ in 0..6 {
            selected.push(
                scheduler
                    .take(
                        || {
                            calls.borrow_mut().push(false);
                            Ok::<_, ()>(Some("new input"))
                        },
                        || {
                            calls.borrow_mut().push(true);
                            Ok(Some("old history"))
                        },
                    )
                    .unwrap()
                    .unwrap()
                    .0,
            );
        }
        assert_eq!(selected, [false, true, false, true, false, true]);
        assert_eq!(*calls.borrow(), selected);
    }

    #[test]
    fn exhausted_maintenance_does_not_block_normal_and_exhausted_normal_keeps_maintenance() {
        let mut scheduler = AlternatingBatches::default();
        for _ in 0..3 {
            assert_eq!(
                scheduler.take(|| Ok::<_, ()>(Some(1)), || Ok(None)),
                Ok(Some((false, 1)))
            );
        }
        for _ in 0..3 {
            assert_eq!(
                scheduler.take(|| Ok::<_, ()>(None), || Ok(Some(2))),
                Ok(Some((true, 2)))
            );
        }
        assert_eq!(
            scheduler.take(|| Ok::<_, ()>(Some(3)), || panic!("new input has priority")),
            Ok(Some((false, 3)))
        );
    }

    #[test]
    fn idle_polls_preserve_the_other_queues_priority() {
        let mut scheduler = AlternatingBatches::default();
        scheduler
            .take(|| Ok::<_, ()>(Some(1)), || Ok(None))
            .unwrap();
        assert!(scheduler.maintenance_next);
        assert_eq!(
            scheduler.take(|| Ok::<Option<u8>, ()>(None), || Ok(None)),
            Ok(None)
        );
        assert!(scheduler.maintenance_next);
        assert_eq!(
            scheduler.take(
                || panic!("maintenance has priority"),
                || Ok::<_, ()>(Some(2))
            ),
            Ok(Some((true, 2)))
        );
    }

    #[test]
    fn claim_error_stops_without_claiming_the_other_queue_or_losing_priority() {
        let mut scheduler = AlternatingBatches::default();
        assert_eq!(
            scheduler.take(
                || Err::<Option<u8>, _>("storage unknown"),
                || panic!("must stop")
            ),
            Err("storage unknown")
        );
        assert!(!scheduler.maintenance_next);
    }
}
