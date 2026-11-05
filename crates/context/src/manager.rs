//! 上下文管理器。成功的准备先做 L0 去重，再检查硬上限。
//!
//! **作用**：Engine 每次模型调用前调用 [`ContextManager::prepare`]。校验器调用 [`ContextManager::replay`]。
//! **边界**：不读 blob，不改调用预算，不发请求。超硬上限时返回记录，不改事实。

use fluxaxiom_contract::TokenCount;

use crate::forward_dedup::{ForwardDedup, L0_STRATEGY};
use crate::supersede::{SupersedeRead, L1_STRATEGY};
use crate::profile::ModelProfile;
use crate::record::{ContextRecord, ContextSpec, Estimate, EstimateSource, ReplayError};
use crate::strategy::{ContextStrategy, Identity, IDENTITY_STRATEGY};
use crate::text::ceil_tokens;
use crate::transcript::{Round, Transcript, View};

/// 一次通过硬上限的准备结果。
///
/// **作用**：调用方把 `view` 序列化成模型请求，把 `record` 写成 blob。
/// **边界**：L0 可能把最后一轮的重复工具正文换成存根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// 发给模型的视图。L0 可能把最后一轮的重复工具正文换成存根。`Debug` 只打印长度。
    pub view: View,
    /// 这次 `prepare` 的记录。只有数值和引用，调用方以后把它写成 blob。
    pub record: ContextRecord,
}

/// 估算超过硬上限，或已经没有可用输入空间。
///
/// **作用**：调用方不发这次模型请求。`record` 里有估算和硬上限。
/// **边界**：`Debug` 与 `Display` 只有数值，没有正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextExhausted {
    /// 已经算完的记录，含估算和硬上限。调用方不发请求。`Debug` 与 `Display` 只有这些数值。
    pub record: ContextRecord,
}

/// 持有一份档案的恒等管理器。
///
/// **作用**：成功记录和耗尽记录的策略名都是 `l0@1`。
/// **边界**：不读取档案里没有取值的字段，也不读取单价。耗尽时不发送请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextManager {
    /// 这次使用的模型档案。所有权在管理器里，运行中不替换。
    profile: ModelProfile,
}

impl ContextManager {
    /// 用一份已载入的档案构造管理器。
    ///
    /// **作用**：P1-3 将对 `deepseek_flash()` 的结果调用这里。
    /// **参数**：`profile` 决定窗口、误差和字符比例。
    /// **边界**：不在这里访问网络或磁盘。嵌入档案的解析由调用方完成。
    pub fn new(profile: ModelProfile) -> Self {
        Self { profile }
    }

    /// 先做 L0 去重，再按替换后的正文估算并检查硬上限。
    ///
    /// **作用**：每次模型调用前调用。通过硬上限时，最后一轮的重复工具正文已换成存根。
    /// **参数**：`transcript` 由调用方填入正文。`max_tokens` 是这次请求的输出上限，用来计算硬上限。
    /// **边界**：估算使用替换后的工具正文。更早轮次的已知 prompt 仍是下界。缓存字段不参加加法。
    /// 硬上限为 0，或去重后的估算仍超过硬上限时，返回 [`ContextExhausted`]，不发送请求。
    /// 耗尽记录的策略名也是 `l0@1`。
    pub fn prepare(
        &self,
        transcript: &Transcript,
        max_tokens: u32,
    ) -> Result<Prepared, ContextExhausted> {
        let spec = ContextSpec {
            hard_limit: self.profile.hard_limit(max_tokens),
        };
        let mut view = View::identity(transcript);
        let mut record = ContextRecord::identity(
            self.profile.profile_version,
            self.profile.profile_hash.clone(),
            spec.clone(),
            Estimate {
                tokens: 0,
                source: EstimateSource::Estimated,
                anchor_round: None,
            },
        );
        record.context_strategy = L0_STRATEGY.to_owned();
        ForwardDedup.apply(&mut view, &spec, &mut record);
        let mut estimated = transcript.clone();
        for (round, view_round) in estimated.rounds.iter_mut().zip(&view.rounds) {
            for (tool, view_tool) in round.tools.iter_mut().zip(&view_round.tools) {
                tool.body = view_tool.body.clone();
            }
        }
        record.estimate = estimate_transcript(&self.profile, &estimated);
        if record.spec.hard_limit == 0 || record.estimate.tokens > record.spec.hard_limit {
            return Err(ContextExhausted { record });
        }
        Ok(Prepared { view, record })
    }

    /// 用事实和记录重建视图。
    ///
    /// **作用**：校验器拿它的结果做逐字节比较。本函数不理解编辑种类。
    /// **参数**：`transcript` 是调用方从 blob 装好的事实。`record` 是当时的 context 记录。
    /// **边界**：接受 schema 1 的 `identity@1` 与 `l0@1`。不可重放、未知策略或恒等记录里有编辑时返回错误。
    /// 检查估算来源与锚点的关联及轮次范围；不重新计算估算，也不对照硬上限。
    pub fn replay(transcript: &Transcript, record: &ContextRecord) -> Result<View, ReplayError> {
        if record.schema_version != crate::record::SCHEMA_VERSION {
            return Err(ReplayError::UnsupportedSchema {
                schema_version: record.schema_version,
            });
        }
        if !record.replayable {
            return Err(ReplayError::NotReplayable);
        }
        record.estimate.validate()?;
        if record
            .estimate
            .anchor_round
            .is_some_and(|round| u64::from(round) > transcript.rounds.len() as u64)
        {
            return Err(ReplayError::InvalidEstimate);
        }
        let mut view = View::identity(transcript);
        if record.context_strategy == IDENTITY_STRATEGY {
            Identity.replay(&mut view, record)?;
        } else if record.context_strategy == L0_STRATEGY {
            ForwardDedup.replay(&mut view, record)?;
        } else if record.context_strategy == L1_STRATEGY {
            SupersedeRead.replay(&mut view, record)?;
        } else {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        Ok(view)
    }
}

/// 选择锚点估算或全量估算，并以已知 prompt 用量托底。
/// 估算下一次模型请求需要多少输入token,并防止估算结果低于模型服务端已经报告过的真实用量
///
/// **作用**：`prepare` 写入记录前调用。
/// **参数**：`profile` 提供字符比例。`transcript` 提供正文和各轮用量。
/// **边界**：完整锚点逻辑由 `estimate_with_complete_usage` 保留。恒等视图保留全部历史，
/// 因此任何一轮已知的 prompt 都是下一次请求的下界。下界胜出时记为 usage 并记录该轮；
/// completion 仍未知，不把它补成 0，也不把下界当作完整 token 计数。
fn estimate_transcript(profile: &ModelProfile, transcript: &Transcript) -> Estimate {
    let mut estimate = estimate_with_complete_usage(profile, transcript);
    for (index, round) in transcript.rounds.iter().enumerate() {
        if let TokenCount::Known(prompt) = round.usage.prompt_tokens {
            if prompt > estimate.tokens {
                if let Ok(anchor_round) = u32::try_from(index + 1) {
                    estimate = Estimate {
                        tokens: prompt,
                        source: EstimateSource::Usage,
                        anchor_round: Some(anchor_round),
                    };
                }
            }
        }
    }
    estimate
}

/// 从完整用量锚点和全量字符估算中选择估算。
///
/// **作用**：保留完整锚点加新增内容的估算，供 prompt 下界托底。
/// **参数**：`profile` 提供字符比例。`transcript` 提供正文与用量。
/// **边界**：完整锚点要求 prompt 与 completion 都已知。锚点之后缺用量时与全量估算取大；
/// 没有完整锚点时使用全量估算。
fn estimate_with_complete_usage(profile: &ModelProfile, transcript: &Transcript) -> Estimate {
    let Some(index) = transcript
        .rounds
        .iter()
        .rposition(|round| known_usage(round).is_some())
    else {
        return full_estimate(profile, transcript);
    };
    let Some((prompt, completion)) = known_usage(&transcript.rounds[index]) else {
        return full_estimate(profile, transcript);
    };
    let Ok(anchor_round) = u32::try_from(index + 1) else {
        return full_estimate(profile, transcript);
    };
    let delta = ceil_tokens(
        transcript.count_after(index),
        profile.han_num,
        profile.han_den,
        profile.letter_num,
        profile.letter_den,
        profile.digit_num,
        profile.digit_den,
    );
    let anchored = Estimate {
        tokens: prompt.saturating_add(completion).saturating_add(delta),
        source: EstimateSource::Usage,
        anchor_round: Some(anchor_round),
    };
    if index + 1 == transcript.rounds.len() {
        return anchored;
    }
    let full = full_estimate(profile, transcript);
    if anchored.tokens >= full.tokens {
        anchored
    } else {
        full
    }
}

/// 一轮响应里 prompt 与 completion 都已知时返回这两个数。
///
/// **作用**：判断这一轮能不能当锚点。
/// **参数**：`round` 是对话里的一轮。只读它的 `prompt_tokens` 和 `completion_tokens`。
/// **边界**：缓存命中与未命中不参与判断。任一项为 `Unknown` 时返回 `None`。
fn known_usage(round: &Round) -> Option<(u64, u64)> {
    match (round.usage.prompt_tokens, round.usage.completion_tokens) {
        (TokenCount::Known(prompt), TokenCount::Known(completion)) => Some((prompt, completion)),
        _ => None,
    }
}

/// 对下一次请求里的可计数字符做一次向上取整。
///
/// **作用**：没有锚点时的估算。
/// **参数**：`profile` 的比例来自档案。`transcript` 的全部文字都参加计数。
/// **边界**：来源标成 `estimated`，锚点轮次为空。
fn full_estimate(profile: &ModelProfile, transcript: &Transcript) -> Estimate {
    Estimate {
        tokens: ceil_tokens(
            transcript.full_count(),
            profile.han_num,
            profile.han_den,
            profile.letter_num,
            profile.letter_den,
            profile.digit_num,
            profile.digit_den,
        ),
        source: EstimateSource::Estimated,
        anchor_round: None,
    }
}

impl std::fmt::Display for ContextExhausted {
    /// 打印估算 token 和硬上限。
    ///
    /// **作用**：说明这次为什么不发请求。
    /// **参数**：`f` 是 `Display` 的输出目标。只写入估算 token 和硬上限。
    /// **边界**：不含 prompt、工具正文或思维链。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "context exhausted: estimate {} hard_limit {}",
            self.record.estimate.tokens, self.record.spec.hard_limit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxaxiom_contract::{ModelUsage, TokenCount};
    use serde_json::json;

    use crate::profile::deepseek_flash;
    use crate::transcript::{AssistantTurn, Round, ToolCall, ToolResult, Transcript};

    /// 构造四项用量。`prompt` 和 `completion` 写成已知数，用来放锚点。
    ///
    /// **作用**：测试里少写重复的 `ModelUsage`。
    /// **参数**：`prompt` 是已知的 `prompt_tokens`，0 允许。`completion` 是已知的 `completion_tokens`，0 允许。`hit` 和 `miss` 是缓存命中与未命中，传 `Unknown` 用来证明它们不阻断锚点。
    /// **边界**：不接受把 prompt 或 completion 标成未知；那种用量由 [`unknown_completion`] 另造。
    fn usage(prompt: u64, completion: u64, hit: TokenCount, miss: TokenCount) -> ModelUsage {
        ModelUsage {
            prompt_tokens: TokenCount::Known(prompt),
            completion_tokens: TokenCount::Known(completion),
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
        }
    }

    /// 载入嵌入的 DeepSeek Flash 档案。
    ///
    /// **作用**：各测试共用同一份管理器。
    /// **边界**：档案解析失败时测试直接失败。
    fn manager() -> ContextManager {
        ContextManager::new(deepseek_flash().expect("profile"))
    }

    /// 没有模型轮的对话，用来测全量估算。
    ///
    /// **作用**：只放 system 和首条 user。
    /// **参数**：`system` 和 `user` 按原样进入事实。空串允许。测试负责放汉字、字母或哨兵。
    /// **边界**：不填工具结果，轮次为空。
    fn bare(system: &str, user: &str) -> Transcript {
        Transcript {
            system: system.to_owned(),
            user: user.to_owned(),
            rounds: Vec::new(),
        }
    }

    /// 没有上一轮时，汉字、英文字母和数字合在一起向上取整，标点不计。
    #[test]
    fn full_estimate_counts_han_letters_and_digits() {
        let prepared = manager()
            .prepare(&bare("Hi", "你好，123"), 32_768)
            .expect("under limit");
        assert_eq!(prepared.record.estimate.tokens, 4);
        assert_eq!(prepared.record.estimate.source, EstimateSource::Estimated);
        assert_eq!(prepared.record.estimate.anchor_round, None);
        assert_eq!(prepared.record.spec.hard_limit, 711_065);
        assert_eq!(prepared.view.system, "Hi");
        assert_eq!(prepared.view.user, "你好，123");
        assert!(prepared.record.edits.is_empty());
        assert!(prepared.record.replayable);
        assert_eq!(prepared.record.context_strategy, L0_STRATEGY);
    }

    /// 造一轮只有一次 `read_file` 的模型轮。
    ///
    /// **作用**：准备测试放两条引用相同、正文不同的结果。
    /// **参数**：`body` 是这条工具结果的原文，允许空串。
    /// **边界**：参数固定为 `{"path":"a.rs"}`，引用固定为 `sha256:abc`。用量是已知的 10 和 1。
    fn read_round(body: &str) -> Round {
        Round {
            assistant: AssistantTurn {
                content: "keep".to_owned(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
            },
            tools: vec![ToolResult {
                call_id: "call".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"a.rs"}"#.to_owned(),
                result_ref: "sha256:abc".to_owned(),
                byte_count: 1,
                body: body.to_owned(),
            }],
            usage: usage(10, 1, TokenCount::Unknown, TokenCount::Unknown),
            trailing_user: String::new(),
        }
    }

    /// 重复的新工具正文换成存根，重放得到同一份视图。
    #[test]
    fn prepare_stubs_a_duplicate_and_replay_matches() {
        let transcript = Transcript {
            system: "s".to_owned(),
            user: "u".to_owned(),
            rounds: vec![read_round("old"), read_round("new")],
        };
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        assert_eq!(prepared.record.context_strategy, L0_STRATEGY);
        assert_eq!(prepared.view.rounds[0].tools[0].body, "old");
        assert_eq!(prepared.view.rounds[0].assistant.content, "keep");
        assert_eq!(
            prepared.view.rounds[1].tools[0].body,
            "[unchanged: same result as round #1 call 1]"
        );
        assert_eq!(prepared.record.edits.len(), 1);
        let view = ContextManager::replay(&transcript, &prepared.record).expect("replay");
        assert_eq!(view, prepared.view);
    }

    /// 锚点只加最后一轮的工具正文。assistant 正文和未知的缓存字段不再数一遍。
    #[test]
    fn anchor_adds_only_new_tool_text() {
        let transcript = Transcript {
            system: "system".to_owned(),
            user: "user".to_owned(),
            rounds: vec![Round {
                assistant: AssistantTurn {
                    content: "aaaaaaaaaa".to_owned(),
                    reasoning: "bbbbbbbbbb".to_owned(),
                    tool_calls: Vec::new(),
                },
                tools: vec![ToolResult {
                    call_id: "call-1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: "{}".to_owned(),
                    result_ref: "sha256:abc".to_owned(),
                    byte_count: 4,
                    body: "abcd，。".to_owned(),
                }],
                usage: usage(1_000, 200, TokenCount::Unknown, TokenCount::Unknown),
                trailing_user: String::new(),
            }],
        };
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        assert_eq!(prepared.record.estimate.tokens, 1_202);
        assert_eq!(prepared.record.estimate.source, EstimateSource::Usage);
        assert_eq!(prepared.record.estimate.anchor_round, Some(1));
        let view = ContextManager::replay(&transcript, &prepared.record).expect("replay");
        assert_eq!(view.rounds[0].tools[0].body, "abcd，。");
    }

    /// 没有完整锚点且 completion 未知时，仍保留服务端 prompt 下界。
    #[test]
    fn partial_usage_retains_prompt_lower_bound_without_complete_anchor() {
        let mut transcript = bare("A", "");
        transcript.rounds.push(Round {
            assistant: AssistantTurn {
                content: "BB".to_owned(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
            },
            tools: vec![ToolResult {
                call_id: "c".to_owned(),
                name: "n".to_owned(),
                arguments: String::new(),
                result_ref: "sha256:z".to_owned(),
                byte_count: 1,
                body: "Z".to_owned(),
            }],
            usage: ModelUsage {
                prompt_tokens: TokenCount::Known(50),
                completion_tokens: TokenCount::Unknown,
                prompt_cache_hit_tokens: TokenCount::Known(1),
                prompt_cache_miss_tokens: TokenCount::Known(1),
            },
            trailing_user: String::new(),
        });
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        assert_eq!(prepared.record.estimate.source, EstimateSource::Usage);
        assert_eq!(prepared.record.estimate.anchor_round, Some(1));
        assert_eq!(prepared.record.estimate.tokens, 50);
        assert_eq!(
            transcript.rounds[0].usage.completion_tokens,
            TokenCount::Unknown
        );
    }

    /// 已知 prompt 超限时，即使全文字符估算偏低、completion 缺失也不能放行。
    #[test]
    fn partial_usage_over_limit_is_exhausted() {
        let mut transcript = bare("", &"a".repeat(1_000_000));
        let mut round = round_of("x", "", unknown_completion());
        round.usage.prompt_tokens = TokenCount::Known(800_000);
        transcript.rounds.push(round);
        let err = manager()
            .prepare(&transcript, 393_216)
            .expect_err("exhausted");
        assert_eq!(err.record.spec.hard_limit, 458_752);
        assert_eq!(err.record.estimate.tokens, 800_000);
        assert_eq!(err.record.estimate.source, EstimateSource::Usage);
        assert_eq!(err.record.estimate.anchor_round, Some(1));
    }

    /// 后续轮的已知 prompt 比更早的完整锚点更大时，不能只保留旧锚点。
    #[test]
    fn later_partial_prompt_can_raise_an_earlier_complete_anchor() {
        let mut transcript = bare("", "");
        transcript.rounds.push(round_of(
            "",
            "",
            usage(100, 10, TokenCount::Unknown, TokenCount::Unknown),
        ));
        transcript
            .rounds
            .push(round_of("", "", unknown_completion()));
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        assert_eq!(prepared.record.estimate.tokens, 500_000);
        assert_eq!(prepared.record.estimate.anchor_round, Some(2));
        assert_eq!(prepared.record.estimate.source, EstimateSource::Usage);
        assert!(ContextManager::replay(&transcript, &prepared.record).is_ok());
    }

    /// 全量估算大于已知 prompt 下界时，保留较大的估算，不把未知 completion 补成 0。
    #[test]
    fn partial_usage_does_not_lower_a_larger_full_estimate() {
        let mut transcript = bare("", &"中".repeat(1_000));
        let mut round = round_of("", "", unknown_completion());
        round.usage.prompt_tokens = TokenCount::Known(100);
        transcript.rounds.push(round);
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        assert_eq!(prepared.record.estimate.tokens, 1000);
        assert_eq!(prepared.record.estimate.source, EstimateSource::Estimated);
        assert_eq!(prepared.record.estimate.anchor_round, None);
    }

    /// 没有已知 prompt 时仍使用全量估算，不把未知用量编成数字。
    #[test]
    fn unknown_prompt_and_completion_use_full_estimate() {
        let mut transcript = bare("", "abcd");
        let mut round = round_of("", "", unknown_completion());
        round.usage.prompt_tokens = TokenCount::Unknown;
        transcript.rounds.push(round);
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        assert_eq!(prepared.record.estimate.tokens, 2);
        assert_eq!(prepared.record.estimate.source, EstimateSource::Estimated);
        assert_eq!(prepared.record.estimate.anchor_round, None);
    }

    /// 构造一轮：一段 assistant 正文、一条工具结果和给定用量。
    ///
    /// **作用**：多轮锚点测试少写重复结构。
    /// **参数**：`content` 是 assistant 正文，空串允许。`tool_body` 是工具正文，空串允许。`usage` 原样放进这一轮，可以是未知 completion。
    /// **边界**：思维链为空，没有工具调用参数。每次调用使用不同的 `result_ref`，避免被 L0 当成同一结果。
    fn round_of(content: &str, tool_body: &str, usage: ModelUsage) -> Round {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_REF: AtomicU64 = AtomicU64::new(1);
        let result_ref = format!("sha256:{:x}", NEXT_REF.fetch_add(1, Ordering::Relaxed));
        Round {
            assistant: AssistantTurn {
                content: content.to_owned(),
                reasoning: String::new(),
                tool_calls: Vec::new(),
            },
            tools: vec![ToolResult {
                call_id: "c".to_owned(),
                name: "n".to_owned(),
                arguments: String::new(),
                result_ref,
                byte_count: 1,
                body: tool_body.to_owned(),
            }],
            usage,
            trailing_user: String::new(),
        }
    }

    /// 构造 prompt 已知、completion 未知的用量。
    ///
    /// **作用**：让最后一轮不能当锚点，用来测更早的锚点是否还在。
    /// **边界**：prompt 固定为 500000。缓存两项都是 `Unknown`。
    fn unknown_completion() -> ModelUsage {
        ModelUsage {
            prompt_tokens: TokenCount::Known(500_000),
            completion_tokens: TokenCount::Unknown,
            prompt_cache_hit_tokens: TokenCount::Unknown,
            prompt_cache_miss_tokens: TokenCount::Unknown,
        }
    }

    /// 最后一轮缺 completion 时，沿用更早的已知锚点，不让估算骤降到全量估算。
    #[test]
    fn missing_usage_after_an_anchor_does_not_collapse_the_estimate() {
        let mut transcript = bare("A", "");
        transcript.rounds.push(round_of(
            "",
            "abcd",
            usage(400_000, 100_000, TokenCount::Unknown, TokenCount::Unknown),
        ));
        transcript
            .rounds
            .push(round_of("BBBB", "CCCC", unknown_completion()));
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        // 锚点 500_000。其后新增：第 1 轮工具 4 字母 + 第 2 轮 assistant 4 + 工具 4 = 12 字母 → 4 token。
        assert_eq!(prepared.record.estimate.tokens, 500_004);
        assert_eq!(prepared.record.estimate.source, EstimateSource::Usage);
        assert_eq!(prepared.record.estimate.anchor_round, Some(1));
    }

    /// 锚点估算只加 `trailing_user` 和工具正文。
    #[test]
    fn anchor_counts_trailing_user_text() {
        let mut transcript = bare("", "");
        let mut round = round_of(
            "ignored",
            "",
            usage(100, 10, TokenCount::Unknown, TokenCount::Unknown),
        );
        round.trailing_user = "你好".to_owned();
        transcript.rounds.push(round);
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        // 两个汉字按 1 计是 2。
        assert_eq!(prepared.record.estimate.tokens, 112);
    }

    /// 全量估算计入工具调用的 id、名字和参数。
    #[test]
    fn full_estimate_counts_tool_call_text() {
        let mut transcript = bare("", "");
        transcript.rounds.push(Round {
            assistant: AssistantTurn {
                content: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: "ab".to_owned(),
                    name: "cd".to_owned(),
                    arguments: "ef".to_owned(),
                }],
            },
            tools: Vec::new(),
            usage: ModelUsage {
                prompt_tokens: TokenCount::Unknown,
                completion_tokens: TokenCount::Unknown,
                prompt_cache_hit_tokens: TokenCount::Unknown,
                prompt_cache_miss_tokens: TokenCount::Unknown,
            },
            trailing_user: String::new(),
        });
        let prepared = manager().prepare(&transcript, 32_768).expect("under limit");
        // 6 个字母 1.8，向上取整为 2。
        assert_eq!(prepared.record.estimate.tokens, 2);
        assert_eq!(prepared.record.estimate.source, EstimateSource::Estimated);
    }

    /// 硬上限为 0 时，任何正估算都耗尽，记录里仍没有正文。
    #[test]
    fn estimate_over_hard_limit_returns_the_record() {
        let err = manager()
            .prepare(&bare("", "中"), 1_048_576)
            .expect_err("exhausted");
        assert_eq!(err.record.spec.hard_limit, 0);
        assert_eq!(err.record.estimate.tokens, 1);
        let encoded = serde_json::to_string(&err.record).expect("json");
        assert!(!encoded.contains('中'));
        assert_eq!(
            err.to_string(),
            "context exhausted: estimate 1 hard_limit 0"
        );
    }

    /// 输出预留占满或超过窗口时，空输入和仍不计数的标点也不能绕过耗尽检查。
    #[test]
    fn zero_hard_limit_rejects_zero_estimates() {
        for max_tokens in [1_048_576, 1_048_577] {
            for text in ["", "，。\n"] {
                let err = manager()
                    .prepare(&bare("", text), max_tokens)
                    .expect_err("exhausted");
                assert_eq!(err.record.spec.hard_limit, 0);
                assert_eq!(err.record.estimate.tokens, 0);
                let value = serde_json::to_value(&err.record).expect("json");
                assert_eq!(value["spec"], json!({"hard_limit": 0}));
                assert_eq!(
                    value["estimate"],
                    json!({"tokens": 0, "source": "estimated", "anchor_round": null})
                );
            }
        }
        assert!(manager().prepare(&bare("", ""), 32_768).is_ok());
    }

    /// 估算恰好等于正硬上限时仍可发送，避免零上限修复误改正常边界。
    #[test]
    fn estimate_equal_to_positive_hard_limit_is_allowed() {
        let mut transcript = bare("", "");
        transcript.rounds.push(round_of(
            "",
            "",
            usage(711_065, 0, TokenCount::Unknown, TokenCount::Unknown),
        ));
        let prepared = manager().prepare(&transcript, 32_768).expect("at limit");
        assert_eq!(
            prepared.record.estimate.tokens,
            prepared.record.spec.hard_limit
        );
    }

    /// 成功记录的 JSON 键和取值一次对完。正文哨兵不得出现。
    #[test]
    fn record_json_matches_schema_and_omits_text() {
        let profile = deepseek_flash().expect("profile");
        let transcript = Transcript {
            system: "Hi".to_owned(),
            user: "SENTINEL_BODY_do_not_leak".to_owned(),
            rounds: Vec::new(),
        };
        let prepared = ContextManager::new(profile.clone())
            .prepare(&transcript, 32_768)
            .expect("under limit");
        let value = serde_json::to_value(&prepared.record).expect("value");
        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "profile_version": 2,
                "profile_hash": profile.profile_hash,
                "context_strategy": "l0@1",
                "spec": { "hard_limit": 711065 },
                "estimate": {
                    "tokens": 7,
                    "source": "estimated",
                    "anchor_round": null
                },
                "edits": [],
                "replayable": true
            })
        );
        let encoded = serde_json::to_string(&value).expect("string");
        assert!(!encoded.contains("SENTINEL_BODY_do_not_leak"));
        let parsed: ContextRecord = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed, prepared.record);
    }

    /// 未知策略、不可重放、错误 schema 和带编辑的恒等记录都拒绝。
    #[test]
    fn replay_rejects_records_identity_cannot_rebuild() {
        let transcript = bare("A", "");
        let prepared = manager().prepare(&transcript, 32_768).expect("ok");
        let mut record = prepared.record.clone();
        record.context_strategy = "mask@1".to_owned();
        assert!(matches!(
            ContextManager::replay(&transcript, &record),
            Err(ReplayError::UnknownStrategy { id }) if id == "mask@1"
        ));
        record.context_strategy = IDENTITY_STRATEGY.to_owned();
        record.replayable = false;
        assert!(matches!(
            ContextManager::replay(&transcript, &record),
            Err(ReplayError::NotReplayable)
        ));
        record.replayable = true;
        record.schema_version = 2;
        assert!(matches!(
            ContextManager::replay(&transcript, &record),
            Err(ReplayError::UnsupportedSchema { schema_version: 2 })
        ));
        record.schema_version = 1;
        record.edits.push(json!({"op": "mask"}));
        assert!(matches!(
            ContextManager::replay(&transcript, &record),
            Err(ReplayError::IdentityHasEdits)
        ));
    }

    /// 手工构造的矛盾来源、0 轮次与超出事实范围的锚点同样被 replay 拒绝。
    #[test]
    fn replay_rejects_invalid_estimates_built_without_deserialization() {
        let transcript = bare("A", "");
        let prepared = manager().prepare(&transcript, 32_768).expect("ok");
        for (source, anchor_round) in [
            (EstimateSource::Usage, None),
            (EstimateSource::Usage, Some(0)),
            (EstimateSource::Usage, Some(1)),
            (EstimateSource::Estimated, Some(1)),
        ] {
            let mut record = prepared.record.clone();
            record.estimate.source = source;
            record.estimate.anchor_round = anchor_round;
            assert_eq!(
                ContextManager::replay(&transcript, &record),
                Err(ReplayError::InvalidEstimate)
            );
        }
    }

    /// context 记录不能通过嵌套 estimate 绕过必需键和关联校验。
    #[test]
    fn record_deserialization_rejects_invalid_nested_estimates() {
        let prepared = manager().prepare(&bare("A", ""), 32_768).expect("ok");
        let valid = serde_json::to_value(&prepared.record).expect("json");
        for estimate in [
            json!({"tokens": 1, "source": "usage"}),
            json!({"tokens": 1, "source": "estimated"}),
            json!({"tokens": 1, "source": "usage", "anchor_round": null}),
            json!({"tokens": 1, "source": "estimated", "anchor_round": 1}),
        ] {
            let mut value = valid.clone();
            value["estimate"] = estimate;
            assert!(serde_json::from_value::<ContextRecord>(value).is_err());
        }
    }

    /// `Debug` 不带工具正文哨兵。
    #[test]
    fn debug_hides_tool_body() {
        let transcript = Transcript {
            system: String::new(),
            user: String::new(),
            rounds: vec![Round {
                assistant: AssistantTurn {
                    content: String::new(),
                    reasoning: "SENTINEL_REASONING".to_owned(),
                    tool_calls: Vec::new(),
                },
                tools: vec![ToolResult {
                    call_id: "c".to_owned(),
                    name: "n".to_owned(),
                    arguments: String::new(),
                    result_ref: "sha256:abc".to_owned(),
                    byte_count: 3,
                    body: "SENTINEL_BODY_do_not_leak".to_owned(),
                }],
                usage: usage(1, 1, TokenCount::Unknown, TokenCount::Unknown),
                trailing_user: String::new(),
            }],
        };
        let rendered = format!("{transcript:?} {:?}", transcript.rounds[0].tools[0]);
        assert!(!rendered.contains("SENTINEL_BODY_do_not_leak"));
        assert!(!rendered.contains("SENTINEL_REASONING"));
        assert!(rendered.contains("sha256:abc"));
    }
}
