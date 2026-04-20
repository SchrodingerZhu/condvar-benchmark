#![cfg(target_os = "linux")]

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::rc::Rc;

use libc::c_int;

#[repr(C)]
pub struct RawMuslMutex {
    lock: c_int,
    waiters: c_int,
}

#[repr(C)]
pub struct RawMuslCondvar {
    lock: c_int,
    head: *mut core::ffi::c_void,
    tail: *mut core::ffi::c_void,
}

#[repr(C)]
pub struct RawLlvmMutex {
    futex_word: u32,
}

#[repr(C)]
pub struct RawLlvmOldCondvar {
    qmtx: RawLlvmMutex,
    head: *mut core::ffi::c_void,
    tail: *mut core::ffi::c_void,
}

#[repr(C)]
pub struct RawLlvmNewCondvar {
    waiter_prev: *mut core::ffi::c_void,
    waiter_next: *mut core::ffi::c_void,
    queue_lock: RawLlvmMutex,
}

unsafe extern "C" {
    fn cv_mutex_init(mutex: *mut RawMuslMutex) -> c_int;
    fn cv_mutex_destroy(mutex: *mut RawMuslMutex) -> c_int;
    fn cv_mutex_lock(mutex: *mut RawMuslMutex) -> c_int;
    fn cv_mutex_unlock(mutex: *mut RawMuslMutex) -> c_int;

    fn cv_cond_init(condvar: *mut RawMuslCondvar) -> c_int;
    fn cv_cond_destroy(condvar: *mut RawMuslCondvar) -> c_int;
    fn cv_cond_wait(condvar: *mut RawMuslCondvar, mutex: *mut RawMuslMutex) -> c_int;
    fn cv_cond_signal(condvar: *mut RawMuslCondvar) -> c_int;
    fn cv_cond_broadcast(condvar: *mut RawMuslCondvar) -> c_int;

    fn cvw_mutex_init(mutex: *mut RawMuslMutex) -> c_int;
    fn cvw_mutex_destroy(mutex: *mut RawMuslMutex) -> c_int;
    fn cvw_mutex_lock(mutex: *mut RawMuslMutex) -> c_int;
    fn cvw_mutex_unlock(mutex: *mut RawMuslMutex) -> c_int;

    fn cvw_cond_init(condvar: *mut RawMuslCondvar) -> c_int;
    fn cvw_cond_destroy(condvar: *mut RawMuslCondvar) -> c_int;
    fn cvw_cond_wait(condvar: *mut RawMuslCondvar, mutex: *mut RawMuslMutex) -> c_int;
    fn cvw_cond_signal(condvar: *mut RawMuslCondvar) -> c_int;
    fn cvw_cond_broadcast(condvar: *mut RawMuslCondvar) -> c_int;

    fn llvm_mutex_init(mutex: *mut RawLlvmMutex) -> c_int;
    fn llvm_mutex_destroy(mutex: *mut RawLlvmMutex) -> c_int;
    fn llvm_mutex_lock(mutex: *mut RawLlvmMutex) -> c_int;
    fn llvm_mutex_unlock(mutex: *mut RawLlvmMutex) -> c_int;

    fn llvm_old_cond_init(condvar: *mut RawLlvmOldCondvar) -> c_int;
    fn llvm_old_cond_destroy(condvar: *mut RawLlvmOldCondvar) -> c_int;
    fn llvm_old_cond_wait(condvar: *mut RawLlvmOldCondvar, mutex: *mut RawLlvmMutex) -> c_int;
    fn llvm_old_cond_signal(condvar: *mut RawLlvmOldCondvar) -> c_int;
    fn llvm_old_cond_broadcast(condvar: *mut RawLlvmOldCondvar) -> c_int;

    fn llvm_new_cond_init(condvar: *mut RawLlvmNewCondvar) -> c_int;
    fn llvm_new_cond_destroy(condvar: *mut RawLlvmNewCondvar) -> c_int;
    fn llvm_new_cond_wait(condvar: *mut RawLlvmNewCondvar, mutex: *mut RawLlvmMutex) -> c_int;
    fn llvm_new_cond_signal(condvar: *mut RawLlvmNewCondvar) -> c_int;
    fn llvm_new_cond_broadcast(condvar: *mut RawLlvmNewCondvar) -> c_int;
}

fn check_errno(op: &str, code: c_int) {
    assert_eq!(code, 0, "{op} failed with errno {code}");
}

#[doc(hidden)]
pub trait Backend {
    const NAME: &'static str;

    type RawMutex;
    type RawCondvar;

    unsafe fn init_mutex(raw: *mut Self::RawMutex) -> c_int;
    unsafe fn destroy_mutex(raw: *mut Self::RawMutex) -> c_int;
    unsafe fn lock_mutex(raw: *mut Self::RawMutex) -> c_int;
    unsafe fn unlock_mutex(raw: *mut Self::RawMutex) -> c_int;

    unsafe fn init_condvar(raw: *mut Self::RawCondvar) -> c_int;
    unsafe fn destroy_condvar(raw: *mut Self::RawCondvar) -> c_int;
    unsafe fn wait_condvar(condvar: *mut Self::RawCondvar, mutex: *mut Self::RawMutex) -> c_int;
    unsafe fn signal_condvar(raw: *mut Self::RawCondvar) -> c_int;
    unsafe fn broadcast_condvar(raw: *mut Self::RawCondvar) -> c_int;
}

pub struct Mutex<T, B: Backend> {
    raw: Box<B::RawMutex>,
    value: UnsafeCell<T>,
    _backend: PhantomData<B>,
}

unsafe impl<T: Send, B: Backend> Send for Mutex<T, B> {}
unsafe impl<T: Send, B: Backend> Sync for Mutex<T, B> {}

impl<T, B: Backend> Mutex<T, B> {
    pub fn new(value: T) -> Self {
        let mut raw = Box::<B::RawMutex>::new_uninit();
        let init_code = unsafe { B::init_mutex(raw.as_mut_ptr()) };
        check_errno(B::NAME, init_code);

        Self {
            raw: unsafe { raw.assume_init() },
            value: UnsafeCell::new(value),
            _backend: PhantomData,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T, B> {
        let code = unsafe { B::lock_mutex(self.raw_ptr()) };
        check_errno(B::NAME, code);
        unsafe { MutexGuard::new(self) }
    }

    fn raw_ptr(&self) -> *mut B::RawMutex {
        self.raw.as_ref() as *const B::RawMutex as *mut B::RawMutex
    }
}

impl<T, B: Backend> Drop for Mutex<T, B> {
    fn drop(&mut self) {
        let code = unsafe { B::destroy_mutex(self.raw_ptr()) };
        debug_assert_eq!(code, 0);
    }
}

pub struct MutexGuard<'a, T, B: Backend> {
    mutex: &'a Mutex<T, B>,
    _not_send: PhantomData<Rc<()>>,
}

impl<'a, T, B: Backend> MutexGuard<'a, T, B> {
    unsafe fn new(mutex: &'a Mutex<T, B>) -> Self {
        Self {
            mutex,
            _not_send: PhantomData,
        }
    }

    fn into_mutex(self) -> &'a Mutex<T, B> {
        let mutex = self.mutex;
        mem::forget(self);
        mutex
    }
}

impl<T, B: Backend> Deref for MutexGuard<'_, T, B> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T, B: Backend> DerefMut for MutexGuard<'_, T, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T, B: Backend> Drop for MutexGuard<'_, T, B> {
    fn drop(&mut self) {
        let code = unsafe { B::unlock_mutex(self.mutex.raw_ptr()) };
        check_errno(B::NAME, code);
    }
}

pub struct Condvar<B: Backend> {
    raw: Box<B::RawCondvar>,
    _backend: PhantomData<B>,
}

unsafe impl<B: Backend> Send for Condvar<B> {}
unsafe impl<B: Backend> Sync for Condvar<B> {}

impl<B: Backend> Condvar<B> {
    pub fn new() -> Self {
        let mut raw = Box::<B::RawCondvar>::new_uninit();
        let init_code = unsafe { B::init_condvar(raw.as_mut_ptr()) };
        check_errno(B::NAME, init_code);

        Self {
            raw: unsafe { raw.assume_init() },
            _backend: PhantomData,
        }
    }

    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T, B>) -> MutexGuard<'a, T, B> {
        let mutex = guard.into_mutex();
        let code = unsafe { B::wait_condvar(self.raw_ptr(), mutex.raw_ptr()) };
        check_errno(B::NAME, code);
        unsafe { MutexGuard::new(mutex) }
    }

    pub fn notify_one(&self) {
        let code = unsafe { B::signal_condvar(self.raw_ptr()) };
        check_errno(B::NAME, code);
    }

    pub fn notify_all(&self) {
        let code = unsafe { B::broadcast_condvar(self.raw_ptr()) };
        check_errno(B::NAME, code);
    }

    fn raw_ptr(&self) -> *mut B::RawCondvar {
        self.raw.as_ref() as *const B::RawCondvar as *mut B::RawCondvar
    }
}

impl<B: Backend> Drop for Condvar<B> {
    fn drop(&mut self) {
        let code = unsafe { B::destroy_condvar(self.raw_ptr()) };
        debug_assert_eq!(code, 0);
    }
}

pub struct MuslBackend;

impl Backend for MuslBackend {
    const NAME: &'static str = "musl";

    type RawMutex = RawMuslMutex;
    type RawCondvar = RawMuslCondvar;

    unsafe fn init_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cv_mutex_init(raw) }
    }

    unsafe fn destroy_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cv_mutex_destroy(raw) }
    }

    unsafe fn lock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cv_mutex_lock(raw) }
    }

    unsafe fn unlock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cv_mutex_unlock(raw) }
    }

    unsafe fn init_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cv_cond_init(raw) }
    }

    unsafe fn destroy_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cv_cond_destroy(raw) }
    }

    unsafe fn wait_condvar(condvar: *mut Self::RawCondvar, mutex: *mut Self::RawMutex) -> c_int {
        unsafe { cv_cond_wait(condvar, mutex) }
    }

    unsafe fn signal_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cv_cond_signal(raw) }
    }

    unsafe fn broadcast_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cv_cond_broadcast(raw) }
    }
}

pub struct LibcBackend;

impl Backend for LibcBackend {
    const NAME: &'static str = "libc";

    type RawMutex = libc::pthread_mutex_t;
    type RawCondvar = libc::pthread_cond_t;

    unsafe fn init_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { libc::pthread_mutex_init(raw, ptr::null()) }
    }

    unsafe fn destroy_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { libc::pthread_mutex_destroy(raw) }
    }

    unsafe fn lock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { libc::pthread_mutex_lock(raw) }
    }

    unsafe fn unlock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { libc::pthread_mutex_unlock(raw) }
    }

    unsafe fn init_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { libc::pthread_cond_init(raw, ptr::null()) }
    }

    unsafe fn destroy_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { libc::pthread_cond_destroy(raw) }
    }

    unsafe fn wait_condvar(condvar: *mut Self::RawCondvar, mutex: *mut Self::RawMutex) -> c_int {
        unsafe { libc::pthread_cond_wait(condvar, mutex) }
    }

    unsafe fn signal_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { libc::pthread_cond_signal(raw) }
    }

    unsafe fn broadcast_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { libc::pthread_cond_broadcast(raw) }
    }
}

pub struct MuslWakeBackend;

impl Backend for MuslWakeBackend {
    const NAME: &'static str = "musl_wake";

    type RawMutex = RawMuslMutex;
    type RawCondvar = RawMuslCondvar;

    unsafe fn init_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cvw_mutex_init(raw) }
    }

    unsafe fn destroy_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cvw_mutex_destroy(raw) }
    }

    unsafe fn lock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cvw_mutex_lock(raw) }
    }

    unsafe fn unlock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { cvw_mutex_unlock(raw) }
    }

    unsafe fn init_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cvw_cond_init(raw) }
    }

    unsafe fn destroy_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cvw_cond_destroy(raw) }
    }

    unsafe fn wait_condvar(condvar: *mut Self::RawCondvar, mutex: *mut Self::RawMutex) -> c_int {
        unsafe { cvw_cond_wait(condvar, mutex) }
    }

    unsafe fn signal_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cvw_cond_signal(raw) }
    }

    unsafe fn broadcast_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { cvw_cond_broadcast(raw) }
    }
}

pub struct LlvmOldBackend;

impl Backend for LlvmOldBackend {
    const NAME: &'static str = "llvm_old";

    type RawMutex = RawLlvmMutex;
    type RawCondvar = RawLlvmOldCondvar;

    unsafe fn init_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_init(raw) }
    }

    unsafe fn destroy_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_destroy(raw) }
    }

    unsafe fn lock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_lock(raw) }
    }

    unsafe fn unlock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_unlock(raw) }
    }

    unsafe fn init_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_old_cond_init(raw) }
    }

    unsafe fn destroy_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_old_cond_destroy(raw) }
    }

    unsafe fn wait_condvar(condvar: *mut Self::RawCondvar, mutex: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_old_cond_wait(condvar, mutex) }
    }

    unsafe fn signal_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_old_cond_signal(raw) }
    }

    unsafe fn broadcast_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_old_cond_broadcast(raw) }
    }
}

pub struct LlvmNewBackend;

impl Backend for LlvmNewBackend {
    const NAME: &'static str = "llvm_new";

    type RawMutex = RawLlvmMutex;
    type RawCondvar = RawLlvmNewCondvar;

    unsafe fn init_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_init(raw) }
    }

    unsafe fn destroy_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_destroy(raw) }
    }

    unsafe fn lock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_lock(raw) }
    }

    unsafe fn unlock_mutex(raw: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_mutex_unlock(raw) }
    }

    unsafe fn init_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_new_cond_init(raw) }
    }

    unsafe fn destroy_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_new_cond_destroy(raw) }
    }

    unsafe fn wait_condvar(condvar: *mut Self::RawCondvar, mutex: *mut Self::RawMutex) -> c_int {
        unsafe { llvm_new_cond_wait(condvar, mutex) }
    }

    unsafe fn signal_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_new_cond_signal(raw) }
    }

    unsafe fn broadcast_condvar(raw: *mut Self::RawCondvar) -> c_int {
        unsafe { llvm_new_cond_broadcast(raw) }
    }
}

pub mod musl {
    pub type Condvar = super::Condvar<super::MuslBackend>;
    pub type Mutex<T> = super::Mutex<T, super::MuslBackend>;
    pub type MutexGuard<'a, T> = super::MutexGuard<'a, T, super::MuslBackend>;
}

pub mod system {
    pub type Condvar = super::Condvar<super::LibcBackend>;
    pub type Mutex<T> = super::Mutex<T, super::LibcBackend>;
    pub type MutexGuard<'a, T> = super::MutexGuard<'a, T, super::LibcBackend>;
}

pub mod musl_wake {
    pub type Condvar = super::Condvar<super::MuslWakeBackend>;
    pub type Mutex<T> = super::Mutex<T, super::MuslWakeBackend>;
    pub type MutexGuard<'a, T> = super::MutexGuard<'a, T, super::MuslWakeBackend>;
}

pub mod llvm_old {
    pub type Condvar = super::Condvar<super::LlvmOldBackend>;
    pub type Mutex<T> = super::Mutex<T, super::LlvmOldBackend>;
    pub type MutexGuard<'a, T> = super::MutexGuard<'a, T, super::LlvmOldBackend>;
}

pub mod llvm_new {
    pub type Condvar = super::Condvar<super::LlvmNewBackend>;
    pub type Mutex<T> = super::Mutex<T, super::LlvmNewBackend>;
    pub type MutexGuard<'a, T> = super::MutexGuard<'a, T, super::LlvmNewBackend>;
}
