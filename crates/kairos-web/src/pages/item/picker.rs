//! A picker that reads its list when its control opens (KAIROS-T-0618).
//!
//! The item page had four pickers that read a full list on each mount:
//! the repository and the board move of a task, the owner board and the
//! impacts of a document. Each read every page of the repositories or the
//! boards, also for a person who could not use the picker. Now a closed
//! picker reads nothing, and a button opens it.

use std::future::Future;

use leptos::prelude::*;

/// What a picker shows. Pure, host-tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Picker {
    /// The control is closed: the open button, and no read.
    Closed,
    /// The control is open and the list is not there yet.
    Loading,
    /// The read of the list failed.
    Failed,
    /// The list has no option for this person.
    Empty,
    /// The select, with its options.
    Ready,
}

/// The state of a picker: open or not, the read (`None` while it runs),
/// and the number of options that the read gives. Pure, host-tested.
pub(super) fn picker_state<T, E>(
    open: bool,
    read: Option<&Result<T, E>>,
    options: usize,
) -> Picker {
    match (open, read) {
        (false, _) => Picker::Closed,
        (true, None) => Picker::Loading,
        (true, Some(Err(_))) => Picker::Failed,
        (true, Some(Ok(_))) if options == 0 => Picker::Empty,
        (true, Some(Ok(_))) => Picker::Ready,
    }
}

/// A read that starts when `open` is true. The resource gives `None`
/// while the control is closed, and the read again after a new session.
pub(super) fn load_on_open<T, Fut>(
    open: RwSignal<bool>,
    fetch: impl Fn() -> Fut + 'static,
) -> LocalResource<Option<T>>
where
    T: 'static,
    Fut: Future<Output = T> + 'static,
{
    LocalResource::new(move || {
        let future = open.get().then(&fetch);
        async move {
            match future {
                Some(future) => Some(future.await),
                None => None,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed picker reads nothing and shows the open button, whatever
    /// the last read was.
    #[test]
    fn a_closed_picker_shows_the_button() {
        assert_eq!(picker_state::<(), ()>(false, None, 0), Picker::Closed);
        assert_eq!(
            picker_state::<(), ()>(false, Some(&Ok(())), 3),
            Picker::Closed
        );
    }

    /// An open picker waits for the read, then shows the select, a text
    /// for no option, or a text for a failed read.
    #[test]
    fn an_open_picker_follows_the_read() {
        assert_eq!(picker_state::<(), ()>(true, None, 0), Picker::Loading);
        assert_eq!(
            picker_state::<(), ()>(true, Some(&Err(())), 0),
            Picker::Failed
        );
        assert_eq!(
            picker_state::<(), ()>(true, Some(&Ok(())), 0),
            Picker::Empty
        );
        assert_eq!(
            picker_state::<(), ()>(true, Some(&Ok(())), 2),
            Picker::Ready
        );
    }
}
