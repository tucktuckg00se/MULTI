# Target users and use cases

> **Summary:** Small technical teams without a caption budget: houses of worship, schools, local government, event streamers, small broadcasters, hobbyists. Four key user stories.

The primary buyer is a small technical team that runs live video without a caption budget and is comfortable with a command line or Docker.

| User | Use case | What they need most |
| --- | --- | --- |
| Houses of worship | Weekly services streamed to YouTube/Facebook, often with multilingual congregations | Several languages, low cost, simple setup |
| Schools and universities | Lectures, sports, graduations; accessibility obligations | Accuracy, reliability, on-prem privacy |
| Local government / PEG channels | Council meetings on cable and web | CEA-608/708 compliance, word filter, 24/7 uptime |
| Event and conference streamers | Multi-room events with international audiences | Many simultaneous languages, low latency |
| Small broadcasters and restreamers | Inline box between encoder and CDN | SRT in/out, stable latency, monitoring |
| Hobbyists and developers | Self-hosted streaming, integrations | Free source, ARM (Jetson, Raspberry Pi 5) support |

**Key user stories**

- As an operator, I point MULTI at an SRT source and destination, pick languages, and get a captioned stream in under 5 minutes.
- As a viewer, I choose English, Spanish or French captions in my player from the same stream.
- As a compliance owner, I can be confident no word on our blocklist ever appears in a caption.
- As an engineer, I can watch caption lag, GPU load and dropped frames in a dashboard and get alerted on failures.

