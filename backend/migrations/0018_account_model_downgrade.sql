-- 账号模型降智自动下线：上游静默返回低档位模型（如请求 gpt-6-astra 实际返回
-- gpt-5.6-luna）时，按滑动窗口累计观测次数，达到阈值即把账号下线；此后按固定
-- 间隔执行恢复探测，只有探测返回的模型不再低于请求档位才重新上线。
--
-- 与容量熔断（0010）并行且互不影响：容量熔断针对上游容量类错误，本特性针对
-- 静默降级。默认关闭，需在设置页显式启用。

alter table runtime_settings
    add column account_model_downgrade_enabled boolean not null default false,
    -- 窗口内触发下线的降智观测次数；允许 1 表示首次观测即下线。
    add column account_model_downgrade_threshold bigint not null default 3
        check (account_model_downgrade_threshold between 1 and 1000),
    add column account_model_downgrade_window_seconds bigint not null default 600
        check (account_model_downgrade_window_seconds between 60 and 3600),
    -- 下线后两次恢复探测之间的间隔；默认 1 小时。
    add column account_model_downgrade_probe_interval_seconds bigint not null default 3600
        check (account_model_downgrade_probe_interval_seconds between 300 and 604800),
    -- 模型档位表，最高档在前；只有请求与返回模型都在表内才做比较，
    -- 表外模型一律不判定，避免误伤合法请求低档模型的客户端。
    add column account_model_downgrade_ladder_json jsonb not null
        default '["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]'::jsonb
        check (jsonb_typeof(account_model_downgrade_ladder_json) = 'array'),
    -- 恢复探测使用的模型；为空时使用档位表最高档，最能暴露降智。
    add column account_model_downgrade_probe_model text
        check (
          account_model_downgrade_probe_model is null
          or (
            octet_length(account_model_downgrade_probe_model) between 1 and 128
            and account_model_downgrade_probe_model = btrim(account_model_downgrade_probe_model)
            and account_model_downgrade_probe_model !~ '[[:cntrl:]]'
          )
        );
