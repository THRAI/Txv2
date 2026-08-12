/// Define a typed bus event/readiness bit-set newtype.
///
/// The generated type implements [`WireEventSet`](crate::bus::WireEventSet)
/// and basic bitwise composition. `DECLARED_BITS` is computed as the union of
/// every declared constant.
#[macro_export]
macro_rules! bus_event_set {
    (
        $(#[$type_meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$const_meta:meta])*
                $const_vis:vis const $flag:ident = $bits:expr;
            )+
        }
    ) => {
        $(#[$type_meta])*
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        $vis struct $name(u64);

        impl $name {
            $(
                $(#[$const_meta])*
                $const_vis const $flag: Self = Self(($bits) as u64);
            )+

            $vis const DECLARED_BITS: u64 = 0 $(| (($bits) as u64))+;

            $vis const fn from_bits(bits: u64) -> Self {
                Self(bits)
            }

            $vis const fn bits(self) -> u64 {
                self.0
            }

            $vis const fn is_empty(self) -> bool {
                self.0 == 0
            }

            $vis const fn contains(self, other: Self) -> bool {
                (self.0 & other.0) == other.0
            }
        }

        impl $crate::bus::WireEventSet for $name {
            const DECLARED_BITS: u64 = Self::DECLARED_BITS;

            fn bits(self) -> u64 {
                self.0
            }
        }

        impl ::core::ops::BitOr for $name {
            type Output = Self;

            fn bitor(self, rhs: Self) -> Self::Output {
                Self(self.0 | rhs.0)
            }
        }

        impl ::core::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl ::core::ops::BitAnd for $name {
            type Output = Self;

            fn bitand(self, rhs: Self) -> Self::Output {
                Self(self.0 & rhs.0)
            }
        }

        impl ::core::ops::BitAndAssign for $name {
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }

        impl ::core::ops::Not for $name {
            type Output = Self;

            fn not(self) -> Self::Output {
                Self(Self::DECLARED_BITS & !self.0)
            }
        }
    };
}

/// Define a typed readiness bit-set for a [`DeclaredQueue`](crate::bus::DeclaredQueue).
#[macro_export]
macro_rules! bus_readiness {
    ($($tokens:tt)*) => {
        $crate::bus_event_set! {
            $($tokens)*
        }
    };
}

/// Define a typed lifecycle/event bit-set for a [`DeclaredPort`](crate::bus::DeclaredPort).
#[macro_export]
macro_rules! bus_lifecycle {
    ($($tokens:tt)*) => {
        $crate::bus_event_set! {
            $($tokens)*
        }
    };
}

/// Define a typed payload struct for a [`RawTrace`](crate::bus::RawTrace).
#[macro_export]
macro_rules! bus_tracepoint {
    (
        $(#[$type_meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                $field_vis:vis $field:ident : $field_ty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$type_meta])*
        #[derive(Clone, Copy)]
        $vis struct $name {
            $(
                $(#[$field_meta])*
                $field_vis $field: $field_ty,
            )*
        }

        impl $name {
            $vis const fn new($($field: $field_ty),*) -> Self {
                Self {
                    $($field,)*
                }
            }
        }

        impl $crate::bus::TracePayload for $name {}
    };
}
