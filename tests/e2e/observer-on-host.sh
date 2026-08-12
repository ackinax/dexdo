#!/usr/bin/env bash
# Наблюдатель: сквозные инварианты индексера по всему прогону. Трафика не
# создаёт, читает только развёрнутую БД.
#
# Required env: DEXDO_SHA, PIPELINE_ID, TEST_DATABASE_URL, E2E_ELAPSED_SECONDS.
# Optional: DEXDO_REPO, DEXDO_DIR, OBSERVER_DEADLINE_SECS.
#
# Пайпится по ssh из чекаута CI-раннера, как sdk-proof-on-host.sh, и по той же
# причине: общий чекаут на хосте B — это ровно то состояние, которое скрипт
# собирается перезаписать, и оно может хранить что угодно от прошлого пайплайна.
# Лиз-протокол по той же причине встроен, а не подключается с диска хоста.
set -euo pipefail

: "${DEXDO_SHA:?}" "${PIPELINE_ID:?}" "${TEST_DATABASE_URL:?}" "${E2E_ELAPSED_SECONDS:?}"

case "$PIPELINE_ID" in
  *[[:space:]]*) echo "FATAL: PIPELINE_ID must not contain whitespace: '$PIPELINE_ID'"; exit 64 ;;
esac

# Начало прогона В ЧАСАХ ЭТОГО ХОСТА, и вычисляется оно ПЕРВЫМ делом — до синка.
# `E2E_ELAPSED_SECONDS` снят целиком по часам CI-раннера (разность двух его же
# отсчётов), поэтому смещение часов раннера относительно хоста в результат не
# входит; сравнивается метка с `raw_events.created_at`, который ставит Postgres
# этого же хоста. Если снять `date` после `git fetch`/`checkout`, начало прогона
# уедет вперёд на длительность синхронизации — секунды на тёплом хосте, минуты на
# холодном clone. Направление безопасное (окно у́же, ложного красного нет), но это
# ровно то смещение, которое здесь снимается одной строкой.
E2E_STARTED_AT=$(( $(date +%s) - E2E_ELAPSED_SECONDS ))
export E2E_STARTED_AT TEST_DATABASE_URL

REPO="${DEXDO_REPO:-https://github.com/gosh-sh/dexdo.git}"
DIR="${DEXDO_DIR:-$HOME/dexdo-e2e}"
# shellcheck disable=SC1091  # host-specific path; rustup puts it there on this host's own prior setup
source "$HOME/.cargo/env"

GUARD=/var/lock/dexdo-e2e.lock
LEASE=/var/lock/dexdo-e2e.lease

# Проверка владения — жёсткая, никогда `|| true`. Наблюдатель читает БД стенда;
# если лиз уже не наш, стенд принадлежит другому пайплайну и его база к нашему
# прогону отношения не имеет. Каждый отказ выходит через явный `exit` — по той же
# причине, что и в образце (sdk-proof-on-host.sh): bash снимает `errexit` внутри
# левой части `||`, так что падение сквозь него не остановило бы функцию.
lease_assert() {
  exec 9>"$GUARD"
  flock -w 30 9 || { echo "FATAL: guard busy (flock on $GUARD not acquired within 30s)"; exit 70; }
  if [ -f "$LEASE" ]; then
    read -r L_ID _ < "$LEASE"
  else
    L_ID=""
  fi
  [ "$L_ID" = "$PIPELINE_ID" ] || { echo "FATAL: lease is not ours ($L_ID != $PIPELINE_ID)"; exit 72; }
  echo "$PIPELINE_ID $(date +%s)" > "$LEASE"
  exec 9>&-
}

lease_assert                                   # до первого касания общего чекаута
echo "==> sync dexdo @ ${DEXDO_SHA}"
[ -d "$DIR/.git" ] || git clone "$REPO" "$DIR"
git -C "$DIR" fetch --depth 1 origin "$DEXDO_SHA" && git -C "$DIR" checkout -f FETCH_HEAD
[ "$(git -C "$DIR" rev-parse HEAD)" = "$DEXDO_SHA" ] || { echo "FATAL: checkout HEAD != DEXDO_SHA"; exit 66; }

command -v cargo-nextest >/dev/null 2>&1 \
  || curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C "$HOME/.cargo/bin"

cd "$DIR"
START=$(date +%s)
FILTER='binary(e2e_observer)'

# Дедлайна здесь НЕТ намеренно. Его значение живёт в одном месте — константе
# `DEFAULT_DEADLINE_SECS` теста, — и оттуда же печатается вместе с худшим случаем
# шага (два `#[ignore]`-теста при `--test-threads 1`, то есть сумма дедлайнов).
# Второй источник той же величины разошёлся бы тихо: правка rust-стороны оставила
# бы этот скрипт честно печатать неправду. Чтобы переопределить дедлайн на стенде,
# добавить `OBSERVER_DEADLINE_SECS` в список переменных ssh-вызова в e2e.yml —
# скрипт пробросит его, ничего не зная о значении.

# Пустая выборка — не безобидный no-op: `--run-ignored only` по несовпавшему
# фильтру выходит с нулём, и шаг отчитался бы об успехе, не проверив ничего —
# ровно тот отказ, ради которого наблюдатель и заводится. stderr сохраняется:
# «фильтр ничего не выбрал» и «крейт не собрался» по числу строк неразличимы.
LIST_ERR=$(mktemp)
N=$(cargo nextest list -p dodex-infrastructure --run-ignored only -E "$FILTER" 2>"$LIST_ERR" | grep -c '::' || true)
if [ "$N" -lt 1 ]; then
  echo "FATAL: empty nextest selection for filter $FILTER"
  echo "--- cargo nextest list stderr ---"
  cat "$LIST_ERR"
  rm -f "$LIST_ERR"
  exit 65
fi
rm -f "$LIST_ERR"

lease_assert                                   # непосредственно перед чтением БД
#
# `--profile ci-e2e` берётся не по инерции. Его overrides
# (`.config/nextest.toml`) называют бинари `dodex-api` и сюда не попадают;
# что он реально даёт — `slow-timeout = 60s × terminate-after 10`, то есть жёсткие
# 600 с на тест. Это не дубль дедлайна наблюдателя, а другой предел: дедлайн,
# каким бы он ни был, закрывает НЕ-сходимость, но не зависший запрос —
# `acquire_timeout` ограничивает только установку соединения, а выполнение
# запроса не ограничивает ничто. 600 с и есть эта страховка.
#
# `set +e` вокруг прогона — не стилистика: скрипт под `set -euo pipefail`, и
# упавший `cargo nextest run` завершил бы его прямо на этой строке, так что ни
# `RC=$?`, ни печать бюджета ниже не выполнились бы. Печать нужнее всего как раз
# на красном прогоне, а первый прогон на стенде с наибольшей вероятностью красный.
# Код возврата шага при этом не меняется — он восстанавливается явным `exit`.
set +e
cargo nextest run --profile ci-e2e --color never -p dodex-infrastructure \
  --run-ignored only --test-threads 1 -E "$FILTER"
RC=$?
set -e
# Бюджет пайплайна назван узким. `START` снят до `nextest list`, то есть число
# включает компиляцию: она идёт именно там, а не в `run`.
echo "==> observer wall clock (compile + run): $(( $(date +%s) - START ))s, rc=$RC"
exit "$RC"
