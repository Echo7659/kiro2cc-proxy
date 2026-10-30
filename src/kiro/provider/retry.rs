//! 重试与故障转移：端点选择（select_endpoint）与 call_api_with_retry 多账号多轮重试。
use super::admission::{
    AccountUnavailableError, RateLimitError, StaleContextError, UpstreamResponse,
};
use std::collections::HashMap;
use tokio::time::sleep;

use crate::kiro::endpoint::{BUCKET_THROTTLE_DURATION, Endpoint};
use crate::kiro::model::credentials::KiroCredentials;

use super::core::{KiroProvider, MAX_RETRIES_PER_CREDENTIAL, MAX_TOTAL_RETRIES};

impl KiroProvider {
    pub(crate) fn select_endpoint(
        &self,
        credentials: &KiroCredentials,
        attempt: usize,
    ) -> Option<Endpoint> {
        let region = credentials.effective_api_region(self.token_manager.config());
        let endpoints = credentials.effective_endpoints(region);
        if endpoints.is_empty() {
            return None;
        }
        let len = endpoints.len();
        // 起点 attempt 偏移（attempt % len），轮询保证单账号内多次 attempt 走不同端点
        let start = attempt % len;
        for i in 0..len {
            let candidate = &endpoints[(start + i) % len];
            if !self
                .endpoint_registry
                .is_throttled(credentials.id.unwrap_or(0), candidate.name)
            {
                return Some(candidate.clone());
            }
        }
        None
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个账号最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(可用池大小 × 每账号重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    /// - 当可用池 ≤ 1 时，仅重试 3 次并使用更长退避间隔
    /// - 单账号内 3 次 attempts 之间切换多端点（不消耗切账号配额）
    pub(crate) async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        is_compact: bool,
        thinking_adaptive_requested: bool,
        bound_ids: &[u64],
    ) -> anyhow::Result<(UpstreamResponse, u64)> {
        // 模型感知路由：确保各账号的实际可用模型探测结果是新的。
        // 探测是全局共享的（缓存 + 串行锁），放在并发 permit 之前，
        // 避免冷启动时大量请求占着额度排队等探测
        let model = Self::extract_model_from_request(request_body);
        self.token_manager
            .refresh_model_support_if_stale(model.as_deref(), bound_ids)
            .await;

        // 流式请求的响应体在上游 200 headers 返回后仍持续生成，受 reqwest
        // 总超时约束，统一使用 `UPSTREAM_TIMEOUT_SECS` 长超时档；若使用较短
        // 档位会在长生成中途被掐断（历史分档背景见常量注释）
        let use_long_timeout = is_stream || is_compact;
        let effective_pool = if bound_ids.is_empty() {
            self.token_manager.total_count()
        } else {
            bound_ids.len()
        };
        let max_retries = (effective_pool * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 尝试从请求体中提取会话 ID（模型信息已在上方提取）
        let continuation_id = Self::extract_continuation_id_from_request(request_body);
        // 本次请求内已限流的账号：重试时避开，但不销毁 sticky 绑定
        let mut throttled_in_request: Vec<u64> = Vec::new();
        let mut sends_by_account: HashMap<u64, usize> = HashMap::new();

        for attempt in 0..max_retries {
            // Model fallback is evaluated against the full authorized pool first.
            // Applying it after capacity exclusions could reintroduce a known-unsupported
            // account merely because the supporting account is temporarily unavailable.
            let pool_ids = self
                .token_manager
                .preferred_credential_ids_for_model(model.as_deref(), bound_ids);
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
                if last_error.is_none() && pool_ids.is_empty() {
                    last_error = Some(anyhow::anyhow!(
                        "{}",
                        self.token_manager
                            .describe_unavailable(model.as_deref(), bound_ids)
                    ));
                }
                break;
            }
            // 获取调用上下文（优先路由到同一会话的缓存账号）
            let ctx = match self
                .token_manager
                .acquire_context_sticky(
                    model.as_deref(),
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

            // 选择下一个可用端点（单账号内轮询，跳过被封禁桶）
            let endpoint = match self.select_endpoint(&ctx.credentials, attempt) {
                Some(e) => e,
                None => {
                    let names: Vec<_> = ctx
                        .credentials
                        .effective_endpoints(
                            ctx.credentials
                                .effective_api_region(self.token_manager.config()),
                        )
                        .iter()
                        .map(|e| e.name)
                        .collect();
                    last_error = Some(
                        RateLimitError {
                            retry_after: self.endpoint_registry.next_available_in(ctx.id, &names),
                            reason: "All upstream endpoints are cooling down",
                            credential_id: Some(ctx.id),
                        }
                        .into(),
                    );
                    throttled_in_request.push(ctx.id);
                    continue;
                }
            };
            tracing::debug!(
                "[ENDPOINT] credential={} attempt={} selected={:?} host={}",
                ctx.id,
                attempt + 1,
                endpoint.name,
                endpoint.host
            );

            let url = self.base_url_for(&ctx.credentials, &endpoint);
            let effective_body = Self::rewrite_request_body(
                request_body,
                &ctx.credentials,
                thinking_adaptive_requested,
            );
            let headers = match self.build_headers(
                &ctx,
                &effective_body,
                sends_by_account.get(&ctx.id).copied().unwrap_or(0),
                &endpoint,
            ) {
                Ok(h) => h,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            // 发送请求
            let client = match self.client_for(&ctx.credentials, use_long_timeout) {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            // 请求体可能含敏感上下文，禁止整包入日志，只记录 URL 与长度（cr-result C6）
            tracing::debug!(
                "[KIRO-REQUEST] url={} body_len={}",
                url,
                effective_body.len()
            );
            let response = match self
                .send_admitted(
                    &ctx,
                    client.post(&url).headers(headers).body(effective_body),
                )
                .await
            {
                Ok(resp) => {
                    *sends_by_account.entry(ctx.id).or_default() += 1;
                    resp
                }
                Err(e) => {
                    if let Some(limit) = e.downcast_ref::<RateLimitError>() {
                        tracing::warn!(
                            credential_id = ctx.id,
                            attempt = attempt + 1,
                            reason = limit.reason,
                            retry_after_secs = limit.retry_after_secs(),
                            "上游准入受限，本次请求避让该账号"
                        );
                        throttled_in_request.push(ctx.id);
                        last_error = Some(e);
                        continue;
                    }
                    if e.downcast_ref::<AccountUnavailableError>().is_some() {
                        tracing::warn!(
                            credential_id = ctx.id,
                            attempt = attempt + 1,
                            reason = "account_unavailable",
                            "上游账号已不可用，本次请求避让该账号"
                        );
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
                        "API 请求发送失败（尝试 {}/{}）: {}",
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

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();
            // A late response for replaced/rotated credentials must not charge failures
            // or disable the new identity that now occupies the same account ID.
            if !self.token_manager.context_is_current(&ctx) {
                last_error = Some(StaleContextError.into());
                continue;
            }

            // 402 Payment Required 且额度用尽：禁用账号并故障转移
            if status.as_u16() == 402 && Self::is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用账号并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                let has_available = self.token_manager.report_quota_for_context(&ctx);
                if !has_available {
                    // 全局无可用账号：必须走 describe_unavailable 产出带
                    // QUOTA_EXHAUSTED_ALL_MARKER 的文案，否则这条错误串在
                    // handlers.rs 会被误判进 502 兜底，而非正确的 402。
                    anyhow::bail!(
                        "{}",
                        self.token_manager
                            .describe_unavailable(model.as_deref(), bound_ids)
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换账号无意义；
            // 例外：企业 IdC 账号缺失 profileArn 属确定性配置缺陷——首即禁用
            // （ProfileArnMissing，不做失败计数）并故障转移到其他账号
            if status.as_u16() == 400 {
                if Self::is_profile_arn_required_error(&body) {
                    tracing::warn!(
                        "API 请求失败（账号缺少 profileArn，尝试 {}/{}）: {} {}",
                        attempt + 1,
                        max_retries,
                        status,
                        body
                    );
                    // 确定性配置缺陷：首即禁用（ProfileArnMissing），不做失败计数
                    let has_available = self.token_manager.report_profile_missing_for_context(&ctx);
                    if let Some(ref store) = self.failure_log_store {
                        store.record(ctx.id, "api", status.as_u16(), &body);
                    }
                    if !has_available {
                        anyhow::bail!(
                            "{} API 请求失败（所有账号已用尽）: {} {}",
                            api_type,
                            status,
                            body
                        );
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败: {} {}",
                        api_type,
                        status,
                        body
                    ));
                    continue;
                }
                tracing::warn!(
                    credential_id = ctx.id,
                    attempt = attempt + 1,
                    endpoint = ?endpoint.name,
                    status = status.as_u16(),
                    model = model.as_deref().unwrap_or("unknown"),
                    "上游拒绝请求，停止本次重试"
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是账号/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为账号错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );

                let has_available = self.token_manager.report_failure_for_context(&ctx);
                if let Some(ref store) = self.failure_log_store {
                    store.record(ctx.id, "api", status.as_u16(), &body);
                }
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有账号已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429 Too Many Requests - 限流：驱逐 sticky cache + rotation bias 轮转 + 封禁当前端点桶
            if status.as_u16() == 429 {
                tracing::warn!("Upstream account {} rate limited", ctx.id);
                self.token_manager.report_throttled_for_context(&ctx);
                self.token_manager.report_rotation_for_context(&ctx);
                // 端点级：封禁当前命中桶 30s（不动其它桶，不动账号）
                self.endpoint_registry
                    .throttle(ctx.id, endpoint.name, BUCKET_THROTTLE_DURATION);
                if let Some(cid) = continuation_id.as_deref() {
                    self.token_manager.report_sticky_throttled(cid, ctx.id);
                }
                if !throttled_in_request.contains(&ctx.id) {
                    throttled_in_request.push(ctx.id);
                }
                if let Some(ref store) = self.throttle_log_store {
                    store.record(
                        ctx.id,
                        "api",
                        status.as_u16(),
                        &body,
                        Some(endpoint.name.as_str()),
                    );
                }
                last_error = Some(
                    upstream_throttle
                        .expect("429 headers registered cooldown")
                        .into(),
                );
                // Other accounts may proceed, but every endpoint for this account must honor Retry-After.
                continue;
            }

            // 408/5xx - 瞬态上游错误：重试但不禁用或切换账号
            // （避免 502 high load 等瞬态错误把所有账号锁死）
            if status.as_u16() == 408 || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    sleep(Self::retry_delay(attempt)).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入账号失败
            if status.is_client_error() {
                tracing::warn!(
                    credential_id = ctx.id,
                    attempt = attempt + 1,
                    endpoint = ?endpoint.name,
                    status = status.as_u16(),
                    model = model.as_deref().unwrap_or("unknown"),
                    "上游拒绝请求，停止本次重试"
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换账号）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }
}
