use crate::*;

impl ContextReply {
    /// 身份和形状校验不是对替换后端真实持久化的证明；实现仍须独立读回与正确处理提交不确定。
    pub fn validate_for(
        &self,
        owner: &ContextOwner,
        operation: &ContextOperation,
    ) -> ContextResult<()> {
        owner.validate()?;
        operation.validate()?;
        let valid_view = |view: &ContextView| view.owner == *owner && view.validate().is_ok();
        let valid_revision = |view: &ContextView, expected: u64| {
            view.revision == expected || expected.checked_add(1) == Some(view.revision)
        };
        let valid = match (self, operation) {
            (Self::View { view }, ContextOperation::Inspect {}) => valid_view(view),
            (
                Self::Range { page },
                ContextOperation::ReadRange {
                    revision,
                    offset,
                    limit,
                },
            ) => {
                let end = page.offset.checked_add(page.text.len());
                page.owner == *owner
                    && page.revision == *revision
                    && page.offset == *offset
                    && page.total_bytes <= MAX_NOTE_BYTES
                    && page.text.len() <= *limit
                    && end.is_some_and(|end| {
                        end <= page.total_bytes
                            && page.next_offset == (end < page.total_bytes).then_some(end)
                            && (end == page.total_bytes || !page.text.is_empty())
                    })
                    && match &page.version {
                        None => page.revision == 0 && page.total_bytes == 0,
                        Some(version) => {
                            version.validate().is_ok()
                                && version.revision == page.revision
                                && version.text_bytes == page.total_bytes
                        }
                    }
            }
            (
                Self::Saved {
                    view,
                    saved,
                    effective,
                },
                ContextOperation::Splice {
                    expected_revision,
                    start,
                    text,
                    ..
                },
            ) => {
                *saved
                    && *effective == "next_turn"
                    && valid_view(view)
                    && valid_revision(view, *expected_revision)
                    && start
                        .checked_add(text.len())
                        .and_then(|end| view.text.get(*start..end))
                        .is_some_and(|actual| actual == text)
            }
            (Self::View { view }, ContextOperation::ReadVersion { revision }) => {
                valid_view(view) && view.revision == *revision
            }
            (
                Self::Versions {
                    owner: actual,
                    versions,
                    next_before,
                },
                ContextOperation::Versions { before, limit },
            ) => {
                let mut previous = *before;
                let ordered = versions.iter().all(|v| {
                    let valid = v.validate().is_ok()
                        && previous.is_none_or(|previous| v.revision < previous);
                    previous = Some(v.revision);
                    valid
                });
                let expected_next = (versions.len() == *limit)
                    .then(|| versions.last().map(|v| v.revision))
                    .flatten();
                actual == owner
                    && versions.len() <= *limit
                    && ordered
                    && *next_before == expected_next
            }
            (Self::Preview { preview, saved }, ContextOperation::Preview { text }) => {
                let expected_revision = if preview.changed {
                    preview.base_revision.checked_add(1)
                } else {
                    Some(preview.base_revision)
                };
                !*saved
                    && valid_view(&preview.proposed)
                    && preview.proposed.text == *text
                    && expected_revision == Some(preview.proposed.revision)
                    && preview
                        .proposed
                        .rendered()
                        .is_ok_and(|rendered| rendered == preview.rendered)
            }
            (
                Self::Saved {
                    view,
                    saved,
                    effective,
                },
                ContextOperation::Update {
                    expected_revision,
                    text,
                },
            ) => {
                *saved
                    && *effective == "next_turn"
                    && valid_view(view)
                    && valid_revision(view, *expected_revision)
                    && view.text == *text
            }
            (
                Self::Saved {
                    view,
                    saved,
                    effective,
                },
                ContextOperation::Rollback {
                    expected_revision,
                    target_revision,
                },
            ) => {
                *saved
                    && *effective == "next_turn"
                    && valid_view(view)
                    && valid_revision(view, *expected_revision)
                    && (*target_revision != 0 || view.text.is_empty())
                    && (view.revision == *expected_revision
                        || view.version.as_ref().is_some_and(|v| {
                            v.change
                                == ContextChange::Rollback {
                                    target_revision: *target_revision,
                                }
                        }))
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ContextError::InvalidReply)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner() -> ContextOwner {
        ContextOwner {
            channel: "qq".into(),
            session_id: "s".into(),
            user_id: "u".into(),
        }
    }
    fn view() -> ContextView {
        ContextView {
            owner: owner(),
            revision: 1,
            text: "已读回，尚未发送".into(),
            version: Some(ContextVersion {
                revision: 1,
                sha256: "a".repeat(64),
                text_bytes: "已读回，尚未发送".len(),
                change: ContextChange::Edit {},
            }),
        }
    }
    #[test]
    fn foreign_owner_wrong_revision_and_mismatched_success_receipts_are_rejected() {
        let own = owner();
        let mut foreign = view();
        foreign.owner.user_id = "foreign".into();
        assert!(
            ContextReply::View { view: foreign }
                .validate_for(&own, &ContextOperation::Inspect {})
                .is_err()
        );
        assert!(
            ContextReply::View { view: view() }
                .validate_for(&own, &ContextOperation::ReadVersion { revision: 2 })
                .is_err()
        );
        let operation = ContextOperation::Update {
            expected_revision: 0,
            text: view().text,
        };
        for saved in [false, true] {
            let reply = ContextReply::Saved {
                view: view(),
                saved,
                effective: "next_turn",
            };
            assert_eq!(reply.validate_for(&own, &operation).is_ok(), saved);
            assert!(
                reply
                    .validate_for(
                        &own,
                        &ContextOperation::Update {
                            expected_revision: 4,
                            text: view().text
                        }
                    )
                    .is_err()
            );
            assert!(
                reply
                    .validate_for(
                        &own,
                        &ContextOperation::Update {
                            expected_revision: 0,
                            text: "另一个正文".into()
                        }
                    )
                    .is_err()
            );
        }
    }
    #[test]
    fn preview_must_render_the_exact_bound_candidate_and_never_claim_saved() {
        let own = owner();
        let candidate = view();
        let op = ContextOperation::Preview {
            text: candidate.text.clone(),
        };
        let mut preview = ContextPreview {
            base_revision: 0,
            proposed: candidate.clone(),
            rendered: candidate.rendered().unwrap(),
            changed: true,
        };
        assert!(
            ContextReply::Preview {
                preview: preview.clone(),
                saved: false
            }
            .validate_for(&own, &op)
            .is_ok()
        );
        assert!(
            ContextReply::Preview {
                preview: preview.clone(),
                saved: true
            }
            .validate_for(&own, &op)
            .is_err()
        );
        preview.rendered = Some("伪造另一个已授权内容".into());
        assert!(
            ContextReply::Preview {
                preview,
                saved: false
            }
            .validate_for(&own, &op)
            .is_err()
        );
    }
    #[test]
    fn version_pages_bind_owner_order_cursor_and_rollback_origin() {
        let own = owner();
        let version = view().version.unwrap();
        let op = ContextOperation::Versions {
            before: None,
            limit: 1,
        };
        assert!(
            ContextReply::Versions {
                owner: own.clone(),
                versions: vec![version.clone()],
                next_before: Some(1)
            }
            .validate_for(&own, &op)
            .is_ok()
        );
        assert!(
            ContextReply::Versions {
                owner: own.clone(),
                versions: vec![version.clone()],
                next_before: Some(2)
            }
            .validate_for(&own, &op)
            .is_err()
        );
        assert!(
            ContextReply::Versions {
                owner: own.clone(),
                versions: vec![version.clone(), version],
                next_before: None
            }
            .validate_for(
                &own,
                &ContextOperation::Versions {
                    before: None,
                    limit: 2
                }
            )
            .is_err()
        );
        assert!(
            ContextReply::Saved {
                view: view(),
                saved: true,
                effective: "next_turn"
            }
            .validate_for(
                &own,
                &ContextOperation::Rollback {
                    expected_revision: 0,
                    target_revision: 0
                }
            )
            .is_err()
        );
    }
}
