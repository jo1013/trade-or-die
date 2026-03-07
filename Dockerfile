# 최신 안정 버전의 Rust 사용 (의존성 라이브러리 요구사항 충족)
FROM rust:slim

WORKDIR /app

# 빌드 및 실행에 필요한 의존성 설치
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    g++ \
    && rm -rf /var/lib/apt/lists/*

# 설정 파일 복사
COPY Cargo.toml Cargo.lock* ./

# 의존성 캐싱을 위한 더미 빌드
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -f target/release/deps/polymarket*

# 실제 소스 코드 복사 및 빌드
COPY src ./src
RUN cargo build --release

# 실행 파일 위치 고정
RUN cp target/release/polymarket ./argo-agent

# 환경 변수 파일 생성
RUN touch .env

CMD ["./argo-agent"]
