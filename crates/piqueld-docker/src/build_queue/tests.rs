use super::{BuildPriority::*, *};
use futures_util::poll;
use std::pin::pin;

#[tokio::test]
async fn environment_builds_go_ahead_of_waiting_preview_builds_but_never_interrupt_them() {
    let queue = BuildQueue::new(NonZeroU8::MIN);
    let running = queue.turn(Preview).await;
    let mut preview = pin!(queue.turn(Preview));
    let mut environment = pin!(queue.turn(Environment));
    assert!(poll!(&mut preview).is_pending());
    // The running preview build keeps its turn.
    assert!(poll!(&mut environment).is_pending());
    drop(running);
    let environment = environment.await;
    assert!(poll!(&mut preview).is_pending());
    drop(environment);
    drop(preview.await);
    // A cancelled build leaves its queue.
    let running = queue.turn(Environment).await;
    let mut cancelled = Box::pin(queue.turn(Environment));
    let mut waiting = pin!(queue.turn(Preview));
    assert!(poll!(&mut cancelled).is_pending());
    assert!(poll!(&mut waiting).is_pending());
    drop(cancelled);
    drop(running);
    waiting.await;
}

#[tokio::test]
async fn builds_run_up_to_the_slots_at_once() {
    let queue = BuildQueue::new(NonZeroU8::new(2).expect("two is not zero"));
    let first = queue.turn(Preview).await;
    let second = queue.turn(Environment).await;
    let mut third = pin!(queue.turn(Environment));
    assert!(poll!(&mut third).is_pending());
    drop(first);
    let third = third.await;
    drop((second, third));
    // Both slots are free again.
    let mut first = pin!(queue.turn(Preview));
    let mut second = pin!(queue.turn(Preview));
    assert!(poll!(&mut first).is_ready());
    assert!(poll!(&mut second).is_ready());
}

#[tokio::test]
async fn a_stream_of_environment_builds_never_starves_a_waiting_preview_build() {
    let queue = BuildQueue::new(NonZeroU8::MIN);
    let mut running = queue.turn(Environment).await;
    let mut preview = pin!(queue.turn(Preview));
    assert!(poll!(&mut preview).is_pending());
    for _ in 0..ENVIRONMENT_BURST {
        let mut next = Box::pin(queue.turn(Environment));
        assert!(poll!(&mut next).is_pending());
        drop(running);
        running = next.await;
        assert!(poll!(&mut preview).is_pending());
    }
    let mut environment = pin!(queue.turn(Environment));
    assert!(poll!(&mut environment).is_pending());
    drop(running);
    let preview = preview.await;
    assert!(poll!(&mut environment).is_pending());
    drop(preview);
    environment.await;
}
