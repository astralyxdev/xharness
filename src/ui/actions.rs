//! Actions shared by menus, dialogs and inline editors. Each component binds
//! the keys it wants in its own key context, so the same Enter or Escape does
//! the right thing wherever focus is.

use gpui::actions;

actions!(xharness, [Confirm, Cancel, SelectNext, SelectPrevious]);
