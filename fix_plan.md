# Argo Agent (Polymarket Bot) - Fix Plan

## Phase 1: 안정성 기반 (Stability Foundation)

- [x] git init 및 초기 커밋 (.env 제외, .gitignore 정비) — git already initialized, .gitignore properly configured
- [x] 에러 핸들링 강화: API 호출 실패 시 재시도 로직 (exponential backoff) 추가 (analyst.rs, trader.rs, scanner.rs)
  - analyst.rs: call_api() now retries up to 3x on 429/5xx/overloaded_error with 500ms/1s/2s backoff
  - scanner.rs: Added get_with_retry() helper; applied to all HTTP GETs (markets, positions, prices)
  - trader.rs: place_market_order() HTTP POST retries 2x on 429/5xx with 500ms/1s backoff
  - Non-retryable errors (400/401/403) return immediately without retry
- [x] scanner.rs: API 타임아웃 및 연결 끊김 시 graceful 복구 (현재 unwrap/panic 가능성 제거)
  - Client::builder() with connect_timeout(10s), request timeout(30s), pool_idle_timeout(90s)
  - get_with_retry() now categorizes errors as timeout/connection/request with detailed logging
  - get_active_markets() JSON parse failures now logged and counted toward consecutive_failures (was silently swallowed)
  - fetch_positions() JSON parse failures now logged instead of silently returning empty
  - fetch_token_price_with_side() body read and JSON parse failures return Ok(None) with logging instead of propagating
- [x] trader.rs: 주문 실행 실패 시 상세 에러 로깅 및 Telegram 알림
  - Client::builder() with connect_timeout(10s), timeout(30s), pool_idle_timeout(90s) - matches scanner.rs
  - Network errors now categorized as timeout/connection/network with order context (side, token, price, amount)
  - Transient errors (429/5xx) log attempt count and order context
  - Response body read failures no longer propagate via `?` - retried instead
  - Successful response JSON parse errors return PARSE_ERROR status instead of crashing
  - Non-retryable client errors (4xx) include HTTP status code in error message
  - All-retries-exhausted path logs final failure summary with full order context
  - Note: Telegram alerts already handled by main.rs callers (new entry + early exit paths)
- [x] database.rs: DuckDB WAL 파일 corruption 대비 자동 백업/복구 로직
  - Startup recovery cascade: WAL removal → retry open → restore from .backup file → fail
  - Also handles DB file corruption without WAL: if .backup exists, restores from it
  - Corrupt files preserved as `.corrupt.{timestamp}` for post-mortem analysis
  - `checkpoint()` method: flushes WAL to main DB file, called every loop iteration
  - `backup()` method: checkpoint + copy DB to `.backup`, called every report interval (~4h)
  - db_path stored in struct for backup/restore path construction
- [x] 구조화된 로깅 시스템 도입 (tracing crate 활용, 현재 println! 기반 → 레벨별 로깅)
  - Added `tracing` + `tracing-subscriber` crates with env-filter support (RUST_LOG)
  - Replaced all 103 println! calls across 7 source files with leveled tracing macros
  - debug!: verbose operational details (balance checks, skip reasons, MA states, trade counts)
  - info!: important events (config, trades, position changes, cycle completions, recharger steps)
  - warn!: errors/retries that are recoverable (API failures, RPC fails, low balance, parse errors)
  - error!: critical failures (order rejections, analysis errors)
  - Structured key-value fields for machine-parseable log output
  - Default level: info (override via RUST_LOG=debug or RUST_LOG=warn)

## Phase 2: 수익률 극대화 - 전략 고도화 (Profit Maximization)

- [x] strategy.rs 리팩토링: Half-Kelly 옵션 추가 (풀 켈리는 변동성이 큼, 하프 켈리로 리스크 감소)
  - Added `kelly_fraction` field to Strategy struct (default 0.5 = half-Kelly)
  - Configurable via `KELLY_FRACTION` env var (range 0.1–1.0)
  - `with_kelly_fraction()` builder method for fluent construction
  - Kelly scaling applied inside `calculate_kelly_bet()` — removed hardcoded `* 0.5` from main.rs
  - 5 unit tests: half vs full equality, no-edge zero, sell-side, extreme prices, clamp bounds
- [x] 멀티팩터 분석: analyst.rs에 시장 볼륨, 유동성, 마감시한을 AI 프롬프트에 추가 투입
  - quick_screen: system prompt now instructs AI to consider volume/liquidity/expiry for probability estimation
  - analyze_market: added "Market factors" section explaining v24, liq, chg24%, end fields with decision guidelines
  - analyze_market: bet_fraction rule added — halve if liquidity < $5k
  - expert_team: fundamentals expert now factors market data into confidence level
  - expert_team: contrarian expert checks chg24% for overcorrection signals
  - expert_team: quant expert gets detailed volume/liquidity/momentum/expiry analysis framework
  - expert_team: leader prompt considers quant's liquidity/volume flags for bet sizing
  - analyze_position: system prompt references entry_q and entry_edge for position review
  - All user messages: renamed "State:" to "Market:" for clarity with AI
- [x] 동적 엣지 임계값: 시장 카테고리별 min_edge 차등 적용 (스포츠 8%, 정치 12%, 크립토 10%)
  - Added `category_edge_thresholds()` function in main.rs: returns (screen_min_edge, trade_min_edge) per category
  - Sports: 6%/8% (more predictable from stats, lower barrier), Crypto/Commodities: 8%/10% (volatile)
  - Politics/Finance: 10%/12% (narrative-driven, harder to price accurately)
  - Screen min_edge: used for Haiku screening and cache checks (lower = more markets pass to analysis)
  - Trade min_edge: used post-analysis for final trade decision (higher = stricter, avoids marginal trades)
  - analyst.rs: `quick_screen()` now accepts `min_edge` parameter instead of hardcoded 0.08
  - analyst.rs: `analyze_market()` system prompt now includes category-specific edge in Rules section
  - strategy.rs: added `calculate_kelly_bet_with_edge()` for category-specific edge threshold
  - Logging enhanced with category and min_edge fields for debugging
  - Unit test: verifies sports threshold is more permissive than politics threshold
- [ ] 포지션 사이징 개선: 현재 포트폴리오 상관관계 고려한 분산 투자 로직
- [ ] 시장 타이밍: 마감 임박 시장(24시간 이내) 필터링 또는 가중치 조정
- [ ] 분석 프롬프트 최적화: 과거 승/패 데이터를 learning_summary로 더 구체적으로 주입

## Phase 3: 수익률 극대화 - 리스크 관리 (Risk Management)

- [ ] 일일 최대 손실 한도 (daily drawdown limit) 구현: 하루 잔고 대비 X% 손실 시 트레이딩 중단
- [ ] 포지션 집중도 관리: 단일 시장/카테고리 최대 비중 제한
- [ ] 동적 TP/SL: 시장 변동성 및 마감시한에 따른 take-profit/stop-loss 조정
- [ ] thesis recheck 로직 개선: 리체크 주기를 edge 크기에 따라 동적 조정
- [ ] 잔고 관리 개선: 최소 잔고 유지 비율 설정 (전체 잔고의 30%는 항상 현금 보유)

## Phase 4: 모니터링 및 분석 (Monitoring & Analytics)

- [ ] 성과 분석 모듈: 카테고리별/기간별 승률, ROI, Sharpe ratio 계산
- [ ] database.rs에 성과 리포트 쿼리 추가 (일간/주간/월간 집계)
- [ ] Telegram 일일 리포트: 매일 정해진 시간에 성과 요약 전송
- [ ] 대시보드 개선 (ui.rs): 현재 포지션, 최근 거래, 누적 수익 표시

## Completed
- [x] Project enabled for Ralph
- [x] Exponential backoff retry logic added to all API calls (analyst.rs, scanner.rs, trader.rs)

## Learnings
- Anthropic API can return `overloaded_error` in the JSON body even with a 200 status - need to check error field and retry
- reqwest body needs `.clone()` for retries since `body()` consumes the string
- Scanner pagination was silently swallowing all errors (`Err(_) => break`) - now tracks consecutive failures and retries individual pages
- reqwest Client::new() has NO timeouts by default - hung connections block forever. Always use Client::builder() with connect_timeout and timeout
- reqwest error has `.is_timeout()` and `.is_connect()` methods for categorizing network failures
- DuckDB `CHECKPOINT` statement flushes WAL to main file - cheap and safe to call frequently
- DuckDB WAL corruption can cascade: even after removing WAL, the main .db file may be inconsistent - always have a backup recovery path
- tracing crate: use `format_args!()` for formatted values in structured fields to avoid allocations (e.g., `balance = format_args!("${:.2}", val)`)
- tracing-subscriber EnvFilter: `try_from_default_env()` reads RUST_LOG; fallback with `.unwrap_or_else()` to set default level
