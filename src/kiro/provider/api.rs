//! Kiro 上游调用公开入口：call_api / call_api_stream / call_mcp（含 MCP 重试）。
use super::admission::{
    AccountUnavailableError, RateLimitError, StaleContextError, UpstreamResponse,
};
use std::collections::HashMap;
use tokio::time::sleep;

use super::core::{KiroProvider, MAX_RETRIES_PER_CREDENTIAL, MAX_TOTAL_RETRIES};

impl KiroProvider {
    pub async fn call_api(
        &self,
        request_body: &str,
        is_compact: bool,
        thinking_adaptive_requested: bool,
        bound_ids: &[u64],
    ) -> anyhow::Result<(UpstreamResponse, u64)> {
        self.call_api_with_retry(
            request_body,
            false,
            is_compact,
            thinking_adaptive_requested,
            bound_ids,
        )
        .await
    }

    /// 发送流式 API 请求
    ///
    /// 支持多账号故障转移：
    /// - 400 Bad Request: 直接返回错误，不计入账号失败
    /// - 401/403: 视为账号/权限问题，计入失败次数并允许故障转移
    /// - 402 MONTHLY_REQUEST_COUNT: 视为额度用尽，禁用账号并切换
    /// - 429/5xx/网络等瞬态错误: 重试但不禁用或切换账号（避免误把所有账号锁死）
    ///
    /// # Arguments
    /// * `request_body` - JSON 格式的请求体字符串
    /// * `is_compact` - 是否为 Claude Code `/compact` 压缩请求
    ///   （超时统一为 `UPSTREAM_TIMEOUT_SECS`，历史分档已移除）
    ///
    /// # Returns
    /// 返回原始的 HTTP Response，调用方负责处理流式数据
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        is_compact: bool,
        thinking_adaptive_requested: bool,
        bound_ids: &[u64],
    ) -> anyhow::Result<(UpstreamResponse, u64)> {
        self.call_api_with_retry(
            request_body,
            true,
            is_compact,
            thinking_adaptive_requested,
            bound_ids,
        )
        .await
    }

    /// 发送 MCP API 请求
    ///
    /// 用于 WebSearch 等工具调用。MCP 调用不涉及 `/compact` 压缩语义，
    /// 与普通请求共用统一超时（`UPSTREAM_TIMEOUT_SECS`）。
    ///
    /// # Arguments
    /// * `request_body` - JSON 格式的 MCP 请求体字符串
    ///
    /// # Returns
    /// 返回原始的 HTTP Response
    pub async fn call_mcp(
        &self,
        request_body: &str,
        bound_ids: &[u64],
    ) -> anyhow::Result<(UpstreamResponse, u64)> {
        self.call_mcp_with_retry(request_body, bound_ids).await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    pub(crate) async fn call_mcp_with_retry(
        &self,
        request_body: &str,
        bound_ids: &[u64],
    ) -> anyhow::Result<(UpstreamResponse, u64)> {
        let effective_pool = if bound_ids.is_empty() {
            self.token_manager.total_count()
        } else {
            bound_ids.len()
        };
        let max_retries = (effective_pool * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;

        let continuation_id = Self::extract_continuation_id_from_request(request_body);
        // 本次请求内已限流的账号：重试时避开，但不销毁 sticky 绑定
        let mut throttled_in_request: Vec<u64> = Vec::new();
        let mut sends_by_account: HashMap<u64, usize> = HashMap::new();

        for attempt in 0..max_retries {
            // Restrict the actual selection pool; the manager's avoid list is deliberately
            // advisory and must not be relied on for hard send budgets or cooldowns.
            let pool_ids: Vec<u64> = self
                .token_manager
                .credential_ids()
                .into_iter()
                .filter(|id| bound_ids.is_empty() || bound_ids.contains(id))
                .collect();
            let candidate_ids: Vec<u64> = pool_ids
                .iter()
                .copied()
                .filter(|id| {
                    sends_by_account.get(id).copied().unwrap_or(0) < MAX_RETRIES_PER_CREDENTIAL
                })
                .filter(|id| !throttled_in_request.contains(id))
                .filter(|id| {
                    self.endpoint_registry
                        .account_cooldown_remaining(*id)
                        .is_none()
                })
                .collect();
            if candidate_ids.is_empty() {
                if let Some(delay) = pool_ids
                    .iter()
                    .filter_map(|id| self.endpoint_registry.account_cooldown_remaining(*id))
                    .min()
                {
                    last_error = Some(
                        RateLimitError {
                            retry_after: delay,
                            reason: "Upstream account cooldown",
                            credential_id: None,
                        }
                        .into(),
                    );
                }
                break;
            }
            // 获取调用上下文（MCP 不涉及模型选择，但同样应用 sticky 路由）
            let ctx = match self
                .token_manager
                .acquire_context_sticky(
                    None,
                    &candidate_ids,
                    continuation_id.as_deref(),
                    &throttled_in_request,
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    if last_error.is_none() {
                        last_error = Some(e);
                    }
                    break;
                }
            };

            let url = self.mcp_url_for(&ctx.credentials);
            let headers = match self
                .build_mcp_headers(&ctx, sends_by_account.get(&ctx.id).copied().unwrap_or(0))
            {
                Ok(h) => h,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            // 发送请求（MCP 调用不涉及 /compact 压缩语义，固定使用普通超时）
            let client = match self.client_for(&ctx.credentials, false) {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            let effective_mcp_body = Self::rewrite_profile_arn(request_body, &ctx.credentials);
            let response = match self
                .send_admitted(
                    &ctx,
                    client.post(&url).headers(headers).body(effective_mcp_body),
                )
                .await
            {
                Ok(resp) => {
                    *sends_by_account.entry(ctx.id).or_default() += 1;
                    resp
                }
                Err(e) => {
                    if e.downcast_ref::<RateLimitError>().is_some()
                        || e.downcast_ref::<AccountUnavailableError>().is_some()
                    {
                        throttled_in_request.push(ctx.id);
                        last_error = Some(e);
                        continue;
                    }
                    if e.downcast_ref::<StaleContextError>().is_some() {
                        last_error = Some(e);
                        continue;
                    }
                    *sends_by_account.entry(ctx.id).or_default() += 1;
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            // Account cooldown remains conservative across token rotation/admin edits;
            // only diagnostic/failure mutations below require the captured identity to match.
            let upstream_throttle = (status.as_u16() == 429)
                .then(|| self.record_upstream_throttle(ctx.id, response.headers()));

            // 成功响应
            if status.is_success() {
                return Ok((response, ctx.id));
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();
            // A late response for replaced/rotated credentials must not charge failures
            // or disable the new identity that now occupies the same account ID.
            if !self.token_manager.context_is_current(&ctx) {
                last_error = Some(StaleContextError.into());
                continue;
            }

            // 402 额度用尽
            if status.as_u16() == 402 && Self::is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_for_context(&ctx);
                if !has_available {
                    // 全局无可用账号：必须走 describe_unavailable 产出带
                    // QUOTA_EXHAUSTED_ALL_MARKER 的文案，否则这条错误串在
                    // handlers.rs 会被误判进 502 兜底，而非正确的 402。
                    anyhow::bail!(
                        "{}",
                        self.token_manager.describe_unavailable(None, bound_ids)
                    );
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换账号无意义；
            // 例外：企业 IdC 账号缺失 profileArn 属确定性配置缺陷——首即禁用
            // （ProfileArnMissing，不做失败计数）并故障转移到其他账号
            if status.as_u16() == 400 {
                if Self::is_profile_arn_required_error(&body) {
                    tracing::warn!(
                        "MCP 请求失败（账号缺少 profileArn，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    // 确定性配置缺陷：首即禁用（ProfileArnMissing），不做失败计数
                    let has_available = self.token_manager.report_profile_missing_for_context(&ctx);
                    if let Some(ref store) = self.failure_log_store {
                        store.record(ctx.id, "mcp", status.as_u16(), &body);
                    }
                    if !has_available {
                        anyhow::bail!("MCP 请求失败（所有账号已用尽）: {} {}", status, body);
                    }
                    last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                    continue;
                }
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 账号问题
            if matches!(status.as_u16(), 401 | 403) {
                let has_available = self.token_manager.report_failure_for_context(&ctx);
                if let Some(ref store) = self.failure_log_store {
                    store.record(ctx.id, "mcp", status.as_u16(), &body);
                }
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有账号已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 429 Too Many Requests - 限流：驱逐 sticky cache + rotation bias 轮转
            if status.as_u16() == 429 {
                tracing::warn!("Upstream account {} rate limited", ctx.id);
                self.token_manager.report_throttled_for_context(&ctx);
                self.token_manager.report_rotation_for_context(&ctx);
                if let Some(cid) = continuation_id.as_deref() {
                    self.token_manager.report_sticky_throttled(cid, ctx.id);
                }
                if !throttled_in_request.contains(&ctx.id) {
                    throttled_in_request.push(ctx.id);
                }
                if let Some(ref store) = self.throttle_log_store {
                    store.record(ctx.id, "mcp", status.as_u16(), &body, None);
                }
                last_error = Some(
                    upstream_throttle
                        .expect("429 headers registered cooldown")
                        .into(),
                );
                // Other accounts may proceed, but every endpoint for this account must honor Retry-After.
                continue;
            }

            // 408/5xx - 瞬态上游错误
            if status.as_u16() == 408 || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }
}
