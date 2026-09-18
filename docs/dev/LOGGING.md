# Логирование jcode

## Назначение

`jcode-logging` записывает локальные диагностические события host daemon и runtime `context-control`. Логирование остаётся локальным и не добавляет telemetry или distributed tracing.

## Формат и путь

Новые записи хранятся в `~/.jcode/logs/jcode-YYYY-MM-DD.log`. Каждая новая запись является одним JSON-объектом с переводом строки, то есть файл использует формат JSONL.

Базовые поля записи:

- `schema_version`: версия схемы записи;
- `timestamp`: локальное время в RFC3339 с миллисекундами;
- `timestamp_ms`: Unix time в миллисекундах;
- `level`: `DEBUG`, `INFO`, `WARN` или `ERROR`;
- `pid`: идентификатор процесса;
- `server`, `session`, `provider`, `model`: доступный logging context;
- `event`: имя structured event;
- `message`: текст обычной записи или имя event;
- остальные поля: sanitized event metadata.

Поля с зарезервированными именами не могут заменить базовые поля. При коллизии имя поля получает префикс `field.`.

## Уровни

По умолчанию записываются `INFO`, `WARN` и `ERROR`. Минимальный уровень задаётся переменной `JCODE_LOG_LEVEL`:

- `debug` или `trace`: все четыре уровня;
- `info`: `INFO` и выше;
- `warn` или `warning`: `WARN` и `ERROR`;
- `error`: только `ERROR`;
- `off`, `none` или `silent`: запись отключена;
- неизвестное значение: безопасное значение по умолчанию `info`.

`JCODE_TRACE` сохраняет совместимость с прежним поведением и принудительно включает `DEBUG`, даже если задан другой порог.

`JCODE_LOG_JSON` больше не нужен для structured events. JSONL является форматом по умолчанию.

## Ротация и хранение

- При смене календарной даты logger переключается на новый `jcode-YYYY-MM-DD.log`.
- Размер активного файла по умолчанию ограничен `16 MiB`.
- Положительное значение `JCODE_LOG_MAX_BYTES` задаёт другой предел.
- При превышении предела текущий файл переносится в первый свободный суффикс `jcode-YYYY-MM-DD.N.log`, начиная с `.1.log`.
- Уже существующие rotated-файлы не перезаписываются.
- При первом запуске новой версии старый text log текущей даты сохраняется в первый свободный rotated-файл перед добавлением JSONL.
- Фоновая cleanup-задача удаляет только файлы `jcode-*.log` и `jcode-desktop-*.log`, старше семи дней. Каталоги, `memory-events-*.jsonl` и другие файлы в каталоге логов не затрагиваются.

## Redaction

Перед записью logger заменяет чувствительные значения на `<redacted>`:

- значения полей `api_key`, `device_code`, `access_token`, `refresh_token`, `client_secret`, `authorization`, `password`, credential-like и соответствующих суффиксов;
- callback, approval, checkout, portal и session URLs в чувствительных полях;
- query-параметры URL в обычных сообщениях;
- значения после `Bearer` и `Token`;
- assignments вида `api_key=value` и `Authorization: value`;
- известные прямые token formats с префиксами Anthropic, OpenRouter, Stripe,
  GitHub, Slack, Google, AWS и JWT-like значения;
- чувствительные ключи внутри JSON tool payload.

Control characters в обычных сообщениях заменяются пробелами. Tool input/output и crash context проходят ту же redaction и ограничиваются по размеру. Structured event ограничивается 64 полями и помечается `fields_truncated`, если вход содержит больше полей.

Никогда не передавайте в logging API секреты намеренно. В частности, не должны попадать в записи API keys, device или magic-link tokens, URL query secrets, Stripe secrets, env-file contents и `Authorization` headers.

## Совместимость API

Сохраняются существующие вызовы `info`, `warn`, `error`, `debug`, `event_*`, `auth_event`, `tool_call`, `crash`, `set_server`, `set_session` и `set_provider_info`. Watchdog продолжает писать свои события через `event_info` и `event_warn`, поэтому его записи получают тот же JSONL формат, threshold и redaction.

## События context-control

Runtime записывает агрегированные события управления контекстом через тот же JSONL logger:

- `CONTEXT_PREFLIGHT` и `CONTEXT_METRICS`: revision, оценки input tokens, размеры system prompt, tools, messages и images, лимиты и выбранное действие;
- `CONTEXT_PROVIDER_REQUEST` и `CONTEXT_PROVIDER_USAGE`: режим запроса, revision, оценки, фактические input/output tokens и cache usage;
- `CONTEXT_PROVIDER_REQUEST_REJECTED` и `CONTEXT_PROVIDER_RESPONSE_REJECTED`: причина отказа и несовпавшие revision;
- `CONTEXT_PROVIDER_RESPONSE_ACCEPTED`: принятая revision и фактическая оценка input tokens, если provider её сообщил;
- `CONTEXT_ACTION_RESULT`: действие, sequence, revision и итог `completed`, `skipped`, `failed` или `rejected`;
- `CONTEXT_COMPACTION_APPLIED`: режим, размеры до и после, сэкономленные tokens, длительность и количество обработанных сообщений;
- `CONTEXT_PRUNE_APPLIED`, `CONTEXT_PRUNE_UNDO_APPLIED` и `CONTEXT_PRUNE`: вид операции, количество элементов, размеры до и после, revision и причина отката или пропуска;
- `CONTEXT_COMPACTION_RECOVERY`, `CONTEXT_PAYLOAD_RECOVERY` и `CONTEXT_NATIVE_COMPACTION_RECOVERY`: безопасное восстановление после переполнения контекста или payload.

Подробные события имеют уровень `DEBUG`, поэтому для полного потока нужно задать `JCODE_LOG_LEVEL=debug` или включить `JCODE_TRACE`. События применения compaction, prune и recovery имеют уровень `INFO` и доступны по умолчанию. В новые события не записываются prompts, responses, изображения, credentials или raw provider errors. Общие поля logger (`server`, `session`, `provider`, `model`) могут добавляться отдельно, если они установлены текущим runtime context.

Эти записи позволяют проверить порядок preflight, provider request, usage и отказов по устаревшей revision. Они не доказывают сами по себе общую экономию реальных provider tokens, правильность смысла summary или качество ответа. Для таких выводов нужны фактический usage и отдельный matched benchmark или semantic freshness test. Встроенный offline analyzer в этом изменении не добавляется: JSONL остаётся источником событий для будущей команды `report` или локального анализа.
