-- 2026 (c) Copyright Contributors to the GOSH DAO. All rights reserved.
--
-- Комментарий 0002 запрещал выводить статус из `deadline` и обещал, что ордер
-- остаётся OPEN до `InferenceOrderExpired`. Второе неверно: continuation,
-- возобновившийся после дедлайна (`InferenceOrderBook.sol:1355-1372`), эмитит
-- ТОЛЬКО `InferenceRefunded` — события истечения для этого пути нет вообще.
--
-- Первый запрет остаётся в силе и сформулирован точнее: сравнивать дедлайн с
-- ЧАСАМИ по-прежнему нельзя, время само по себе ничего не закрывает. Сравнение
-- разрешено ровно в одном месте — при обработке `InferenceRefunded`, где цепь
-- уже сообщила, что ордер из книги исчез, и дедлайн лишь отвечает ПОЧЕМУ:
-- `_finalizeTaker` физически недостижим после дедлайна, обе ветки истечения —
-- до него.

comment on column inference_orders.deadline is
    'Unix seconds after which the book may expire this order; NULL when the chain '
    'value is 0, i.e. good-till-cancel. Never compare it to the clock to derive a '
    'status — elapsed time alone closes nothing. The single exception is the '
    'InferenceRefunded projector: the chain has already removed the order there, and '
    'the deadline only says whether the cause was expiry (continuation expiry emits '
    'no InferenceOrderExpired at all).';
