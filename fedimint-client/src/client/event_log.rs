use fedimint_core::table;
use fedimint_eventlog::EventLogTrimableId;

table!(
    DEFAULT_APPLICATION_EVENT_LOG_POS,
    () => EventLogTrimableId,
    "default-application-event-log-pos",
);
