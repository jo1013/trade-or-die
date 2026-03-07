# Argo Agent - AI Survival Trading Bot

> [@mirae_code](https://www.instagram.com/mirae_code)의 "AI에게 50달러를 맡겼다 48시간 뒤 벌어진일" 영상을 보고 따라 만들었습니다.
> 수익률은... 글쎄? 하지만 AI가 스스로 시장을 분석하고, 판단하고, 거래하고, 살아남으려 한다는 건 확실하다.

Polymarket 예측시장에서 **스스로 생존하는 AI 트레이딩 에이전트**.
Claude AI가 시장을 분석하고, Kelly Criterion으로 사이즈를 계산하고, 자동으로 매매한다.

**Trade or Die** - 수익이 곧 API 비용. 돈을 못 벌면 AI는 말 그대로 죽는다.

## How It Works

```
[Balance Check] → [Market Scan] → [AI Analysis] → [Kelly Sizing] → [Trade]
       ^                                                               |
       └─────────────── Profit → API Cost Coverage ───────────────────┘
```

1. **Scanner** - Polymarket에서 500+ 활성 시장 스캔
2. **Haiku Screening** - 저렴한 모델로 빠르게 필터링 (건당 ~$0.001)
3. **Opus Analysis** - 통과한 시장만 정밀 분석 (확률 추정 + 액션 결정)
4. **Kelly Criterion** - 수학적으로 최적 베팅 사이즈 계산
5. **Auto Trade** - Polymarket CLOB API로 주문 실행
6. **Position Management** - AI가 보유 포지션을 주기적으로 리뷰, 익절/손절 판단

## Architecture

```
Scanner ──→ Analyst ──→ Strategy ──→ Trader
   │            │           │           │
   │      (Claude AI)   (Kelly)    (CLOB API)
   │                                    │
   └──── Governor <─────────────────────┘
             │
         Database (DuckDB)
             │
         Notifier (Telegram)
```

| Module | Role |
|--------|------|
| **Scanner** | Polymarket CLOB API - 시장 스캔, 포지션 조회, 가격 조회 |
| **Analyst** | Claude API - Haiku 스크리닝 + Opus 본분석 + 포지션 리뷰 |
| **Strategy** | Kelly Criterion 기반 최적 베팅 비율 계산 |
| **Governor** | 잔고 관리, API 비용 추적, 생존 판단 |
| **Trader** | CLOB 주문 생성, EIP-712 서명, HMAC 인증 |
| **Recharger** | API 크레딧 자동 충전 파이프라인 |
| **Database** | DuckDB - 거래/분석/잔고 이력 영속 저장 |
| **Notifier** | Telegram Bot 알림 |

## Quick Start

### Prerequisites

- Docker & Docker Compose
- Polymarket 계정 + API Keys
- Anthropic API Key
- Polygon 지갑 (EOA + Proxy)

### Setup

```bash
# 1. Clone
git clone https://github.com/your-repo/argo-agent.git
cd argo-agent

# 2. Configure
cp .env.example .env
# .env 파일에 본인의 키 입력

# 3. Run
./start.sh

# 4. Monitor
./log.sh

# 5. Stop
./stop.sh
```

### Local Build (without Docker)

```bash
cargo build --release
./target/release/polymarket
```

## Configuration

`.env.example` 참고. 주요 설정:

| Variable | Description | Default |
|----------|-------------|---------|
| `ANTHROPIC_MODEL` | 본분석 모델 | `claude-opus-4-6` |
| `API_CREDIT_SEED` | 현재 남은 API 크레딧 ($) | `5.0` |
| `TRADE_CYCLE_SECONDS` | 매매 사이클 간격 | `1800` (30분) |
| `POSITION_CHECK_SECONDS` | 포지션 리체크 간격 | `60` |
| `MIN_TRADE_USDC` | 최소 거래 금액 | `1.0` |
| `MAX_POSITION_RECHECKS` | 사이클당 리체크 수 | `8` |

## Wallet Architecture

```
EOA (Signer)  ──signs orders──→  Polymarket CLOB API
     │
Proxy (Funder) ──holds USDC.e, debited on trades──→  Exchange Contracts
```

- **EOA**: 주문 서명용. 자금 보관 X
- **Proxy**: Polymarket 프록시 지갑. 실제 USDC.e 잔고가 여기에 있음

## Auto-Recharge Pipeline

API 크레딧이 바닥나면 AI가 죽는다. 자동 충전 파이프라인:

```
API Credit < $5 감지
  → Proxy에서 USDC.e 출금 (GnosisSafe)
  → Uniswap V3로 USDC.e → Native USDC 스왑
  → RedotPay 카드로 USDC 입금
  → Anthropic Auto-reload → 크레딧 충전
```

## Data

- `data/argo.db` - DuckDB (거래, 분석, 잔고 이력)
- Docker volume으로 마운트, 재시작해도 데이터 유지

## Key Design Decisions

- **Dual Model Strategy**: Haiku로 스크리닝 (~$0.001/건), Opus로 본분석 - API 비용 최소화
- **RPC Failover**: 8개 Polygon RPC URL 순환 (rate limit 대응)
- **Kelly Criterion**: 수학적 최적 사이징. SELL은 NO-side 기준 (1-price, 1-prob) 계산
- **AI Position Management**: 고정 TP/SL 없이 AI가 동적으로 익절/손절 판단
- **DB Persistence**: 재시작해도 거래 이력 유지 (DROP TABLE 없음)
- **Amount Precision**: maker 소수점 2자리 (granularity 10000), taker 4자리 (100)

## Tech Stack

- **Rust** (edition 2021)
- **Anthropic Claude API** (Haiku + Opus)
- **Polygon** (ethers-rs, EIP-712)
- **DuckDB** (embedded)
- **Docker** + docker-compose
- **Telegram Bot API**
- **ratatui** (TUI dashboard)

## License

MIT
