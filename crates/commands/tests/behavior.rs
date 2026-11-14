use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use commands::{CommandError, CommandRegistry, Context, Cooldown, Responder};
use data::Value;
use discord_rust_commands as commands;

#[test]
fn quoted_arguments_are_preserved() {
    let seen = Arc::new(AtomicBool::new(false));
    let output = Arc::clone(&seen);
    let mut registry = CommandRegistry::new("!");
    registry
        .add(
            "say",
            Vec::<String>::new(),
            move |context: &commands::Context| {
                output.store(context.arguments == ["hello world"], Ordering::SeqCst);
            },
        )
        .unwrap();

    assert!(registry.dispatch("!say \"hello world\""));
    assert!(seen.load(Ordering::SeqCst));
}

#[test]
fn cooldown_is_bounded_and_resettable() {
    let mut cooldown = Cooldown::new(1);
    assert!(cooldown.allow());
    assert!(!cooldown.allow());
    cooldown.reset();
    assert!(cooldown.allow());
}

#[test]
fn checks_can_reject_a_command() {
    let mut registry = CommandRegistry::new("!");
    registry
        .add("admin", Vec::<String>::new(), |_: &Context| {})
        .unwrap();
    registry.add_check(|context: &Context| {
        if context.has_permissions(enums::Permissions::ADMINISTRATOR) {
            Ok(())
        } else {
            Err(CommandError::new("missing permissions"))
        }
    });

    let context = Context::new("admin", Vec::new());
    assert_eq!(
        registry.try_dispatch_with(context),
        Err(CommandError::new("missing permissions"))
    );

    let context =
        Context::new("admin", Vec::new()).with_permissions(enums::Permissions::ADMINISTRATOR);
    assert_eq!(registry.try_dispatch_with(context), Ok(true));
}

#[test]
fn handler_errors_reach_the_command_error_handler() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = Arc::clone(&seen);

    let mut registry = CommandRegistry::new("!");
    registry
        .register(
            commands::CommandDefinition::new(
                "explode",
                Vec::<String>::new(),
                |_: &Context| -> Result<(), &'static str> { Err("boom") },
            )
            .with_error_handler(move |context: &Context, error: &CommandError| {
                output
                    .lock()
                    .unwrap()
                    .push(format!("{}: {error}", context.name));
            }),
        )
        .unwrap();

    assert_eq!(
        registry.try_dispatch("!explode"),
        Err(CommandError::new("boom"))
    );
    assert_eq!(*seen.lock().unwrap(), vec!["explode: boom".to_owned()]);
}

#[test]
fn context_helpers_use_the_configured_responder() {
    #[derive(Default)]
    struct RecordingResponder {
        messages: std::sync::Mutex<Vec<String>>,
    }

    impl Responder for RecordingResponder {
        fn send(&self, channel_id: u64, content: &str) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("send {channel_id}: {content}"));
            Ok(())
        }

        fn reply(
            &self,
            channel_id: u64,
            message_id: u64,
            content: &str,
        ) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("reply {channel_id}/{message_id}: {content}"));
            Ok(())
        }

        fn send_embed(&self, channel_id: u64, _embed: &Value) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("embed {channel_id}"));
            Ok(())
        }

        fn typing(&self, channel_id: u64) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("typing {channel_id}"));
            Ok(())
        }
    }

    let responder = Arc::new(RecordingResponder::default());
    let context = Context::new("say", Vec::new())
        .with_ids(None, Some(7), Some(9), Some(11))
        .with_responder(responder.clone());

    context.respond("hello").unwrap();
    context.reply("hi").unwrap();
    context.respond_embed(&Value::object()).unwrap();
    context.typing().unwrap();

    assert_eq!(
        *responder.messages.lock().unwrap(),
        vec![
            "send 7: hello".to_owned(),
            "reply 7/11: hi".to_owned(),
            "embed 7".to_owned(),
            "typing 7".to_owned(),
        ]
    );
}
