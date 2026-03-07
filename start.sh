#!/bin/bash
# start.sh
echo "🚀 Argo-Rust AI Agent Starting in Background..."
docker-compose up -d --build
echo "✅ Agent is running. Use './log.sh' to see the dashboard."
