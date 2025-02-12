use std::{
    borrow::{Borrow, BorrowMut},
    ffi::c_void,
    marker::PhantomData,
};

use odbc_sys::{CDataType, NULL_DATA};

use crate::{
    buffers::Indicator,
    handles::{CData, CDataMut, HasDataType},
    DataType, OutputParameter,
};

use super::CElement;

/// A tag used to differentiate between different types of variadic buffers.
///
/// # Safety
///
/// * `TERMINATING_ZEROES` is used to calculate buffer offsets.
/// * `C_DATA_TYPE` is used to bind parameters. Providing wrong values like e.g. a fixed length
///   types, would cause even a correctly implemented odbc driver to access invalid memory.
pub unsafe trait VarKind {
    /// Number of terminating zeroes required for this kind of variadic buffer.
    const TERMINATING_ZEROES: usize;
    const C_DATA_TYPE: CDataType;
    fn relational_type(length: usize) -> DataType;
}

/// Intended to be used as a generic argument for [`VariadicCell`] to declare that this buffer is
/// used to hold narrow (as opposed to wide UTF-16) text.
pub struct Text;

unsafe impl VarKind for Text {
    const TERMINATING_ZEROES: usize = 1;
    const C_DATA_TYPE: CDataType = CDataType::Char;

    fn relational_type(length: usize) -> DataType {
        // Since we might use as an input buffer, we report the full buffer length in the type and
        // do not deduct 1 for the terminating zero.
        DataType::Varchar { length }
    }
}

/// Intended to be used as a generic argument for [`VariadicCell`] to declare that this buffer is
/// used to hold raw binary input.
pub struct Binary;

unsafe impl VarKind for Binary {
    const TERMINATING_ZEROES: usize = 0;
    const C_DATA_TYPE: CDataType = CDataType::Binary;

    fn relational_type(length: usize) -> DataType {
        DataType::Varbinary { length }
    }
}

/// Binds a byte array as Variadic sized character data. It can not be used for columnar bulk
/// fetches, but if the buffer type is stack allocated it can be utilized in row wise bulk fetches.
///
/// Meaningful instantiations of this type are:
///
/// * [`self::VarCharSlice`] - immutable borrowed parameter.
/// * [`self::VarCharSliceMut`] - mutable borrowed input / output parameter
/// * [`self::VarCharArray`] - stack allocated owned input / output parameter
/// * [`self::VarCharBox`] - heap allocated owned input /output parameter
/// * [`self::VarBinarySlice`] - immutable borrowed parameter.
/// * [`self::VarBinarySliceMut`] - mutable borrowed input / output parameter
/// * [`self::VarBinaryArray`] - stack allocated owned input / output parameter
/// * [`self::VarBinaryBox`] - heap allocated owned input /output parameter
#[derive(Debug, Clone, Copy)]
pub struct VarCell<B, K> {
    /// Contains the value. Characters must be valid up to the index indicated by `indicator`. If
    /// `indicator` is longer than buffer, the last element in buffer must be a terminating zero,
    /// which is not regarded as being part of the payload itself.
    buffer: B,
    /// Indicates the length of the value stored in `buffer`. Should indicator exceed the buffer
    /// length the value stored in buffer is truncated, and holds actually `buffer.len() - 1` valid
    /// characters. The last element of the buffer being the terminating zero. If indicator is
    /// exactly the buffer length, the value should be considered valid up to the last element,
    /// unless the value is `\0`. In that case we assume `\0` to be a terminating zero left over
    /// from truncation, rather than the last character of the string.
    indicator: isize,
    /// Variadic Kind, declaring wether the buffer holds text or binary data.
    kind: PhantomData<K>,
}

pub type VarBinary<B> = VarCell<B, Binary>;
pub type VarChar<B> = VarCell<B, Text>;

/// Parameter type for owned, variable sized character data.
///
/// We use `Box<[u8]>` rather than `Vec<u8>` as a buffer type since the indicator pointer already
/// has the role of telling us how many bytes in the buffer are part of the payload.
pub type VarCharBox = VarChar<Box<[u8]>>;

/// Parameter type for owned, variable sized binary data.
///
/// We use `Box<[u8]>` rather than `Vec<u8>` as a buffer type since the indicator pointer already
/// has the role of telling us how many bytes in the buffer are part of the payload.
pub type VarBinaryBox = VarBinary<Box<[u8]>>;

impl<K> VarCell<Box<[u8]>, K>
where
    K: VarKind,
{
    /// Constructs a 'missing' value.
    pub fn null() -> Self {
        // We do not want to use the empty buffer (`&[]`) here. It would be bound as `VARCHAR(0)`
        // which caused errors with Microsoft Access and older versions of the Microsoft SQL Server
        // ODBC driver.
        Self::from_buffer(Box::new([0]), Indicator::Null)
    }

    /// Create an owned parameter containing the character data from the passed string.
    pub fn from_string(val: String) -> Self {
        Self::from_vec(val.into_bytes())
    }

    /// Create a VarChar box from a `Vec`.
    pub fn from_vec(val: Vec<u8>) -> Self {
        let indicator = Indicator::Length(val.len());
        let buffer = val.into_boxed_slice();
        Self::from_buffer(buffer, indicator)
    }
}

impl<B, K> VarCell<B, K>
where
    B: Borrow<[u8]>,
    K: VarKind,
{
    /// Creates a new instance from an existing buffer. For text should the indicator be `NoTotal`
    /// or indicate a length longer than buffer, the last element in the buffer must be nul (`\0`).
    pub fn from_buffer(buffer: B, indicator: Indicator) -> Self {
        let buf = buffer.borrow();
        if indicator.is_truncated(buf.len()) {
            // Value is truncated. Let's check that all required terminating zeroes are at the end
            // of the buffer.
            if !ends_in_zeroes(buf, K::TERMINATING_ZEROES) {
                panic!("Truncated value must be terminated with zero.")
            }
        }

        Self {
            buffer,
            indicator: indicator.to_isize(),
            kind: PhantomData,
        }
    }

    /// Valid payload of the buffer (excluding terminating zeroes) returned as slice or `None` in
    /// case the indicator is `NULL_DATA`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        let slice = self.buffer.borrow();
        match self.indicator() {
            Indicator::Null => None,
            Indicator::NoTotal => Some(&slice[..(slice.len() - K::TERMINATING_ZEROES)]),
            Indicator::Length(len) => {
                if self.is_complete() {
                    Some(&slice[..len])
                } else {
                    Some(&slice[..(slice.len() - K::TERMINATING_ZEROES)])
                }
            }
        }
    }

    /// Call this method to ensure that the entire field content did fit into the buffer. If you
    /// retrieve a field using [`crate::CursorRow::get_data`], you can repeat the call until this
    /// method is false to read all the data.
    ///
    /// ```
    /// use odbc_api::{CursorRow, parameter::VarCharArray, Error, handles::Statement};
    ///
    /// fn process_large_text(
    ///     col_index: u16,
    ///     row: &mut CursorRow<'_>
    /// ) -> Result<(), Error>{
    ///     let mut buf = VarCharArray::<512>::NULL;
    ///     row.get_data(col_index, &mut buf)?;
    ///     while !buf.is_complete() {
    ///         // Process bytes in stream without allocation. We can assume repeated calls to
    ///         // get_data do not return `None` since it would have done so on the first call.
    ///         process_text_slice(buf.as_bytes().unwrap());
    ///     }
    ///     Ok(())
    /// }
    ///
    /// fn process_text_slice(text: &[u8]) { /*...*/}
    ///
    /// ```
    ///
    /// ```
    /// use odbc_api::{CursorRow, parameter::VarBinaryArray, Error, handles::Statement};
    ///
    /// fn process_large_binary(
    ///     col_index: u16,
    ///     row: &mut CursorRow<'_>
    /// ) -> Result<(), Error>{
    ///     let mut buf = VarBinaryArray::<512>::NULL;
    ///     row.get_data(col_index, &mut buf)?;
    ///     while !buf.is_complete() {
    ///         // Process bytes in stream without allocation. We can assume repeated calls to
    ///         // get_data do not return `None` since it would have done so on the first call.
    ///         process_slice(buf.as_bytes().unwrap());
    ///     }
    ///     Ok(())
    /// }
    ///
    /// fn process_slice(text: &[u8]) { /*...*/}
    ///
    /// ```
    pub fn is_complete(&self) -> bool {
        let slice = self.buffer.borrow();
        let max_value_length = if ends_in_zeroes(slice, K::TERMINATING_ZEROES) {
            slice.len() - K::TERMINATING_ZEROES
        } else {
            slice.len()
        };
        !self.indicator().is_truncated(max_value_length)
    }

    /// Read access to the underlying ODBC indicator. After data has been fetched the indicator
    /// value is set to the length the buffer should have had to hold the entire value. It may also
    /// be [`Indicator::Null`] to indicate `NULL` or [`Indicator::NoTotal`] which tells us the data
    /// source does not know how big the buffer must be to hold the complete value.
    /// [`Indicator::NoTotal`] implies that the content of the current buffer is valid up to its
    /// maximum capacity.
    pub fn indicator(&self) -> Indicator {
        Indicator::from_isize(self.indicator)
    }

    /// The payload in bytes the buffer can hold including terminating zeroes
    pub fn capacity(&self) -> usize {
        self.buffer.borrow().len()
    }
}

impl<B, K> VarCell<B, K>
where
    B: Borrow<[u8]>,
    K: VarKind,
{
    /// Call this method to reset the indicator to a value which matches the length returned by the
    /// [`Self::as_bytes`] method. This is useful if you want to insert values into the database
    /// despite the fact, that they might have been truncated. Otherwise the behaviour of databases
    /// in this situation is driver specific. Some drivers insert up to the terminating zero, others
    /// detect the truncation and throw an error.
    pub fn hide_truncation(&mut self) {
        if !self.is_complete() {
            self.indicator = (self.buffer.borrow().len() - K::TERMINATING_ZEROES)
                .try_into()
                .unwrap();
        }
    }
}

unsafe impl<B, K> CData for VarCell<B, K>
where
    B: Borrow<[u8]>,
    K: VarKind,
{
    fn cdata_type(&self) -> CDataType {
        K::C_DATA_TYPE
    }

    fn indicator_ptr(&self) -> *const isize {
        &self.indicator as *const isize
    }

    fn value_ptr(&self) -> *const c_void {
        self.buffer.borrow().as_ptr() as *const c_void
    }

    fn buffer_length(&self) -> isize {
        // This is the maximum buffer length, but it is NOT the length of an instance of Self due to
        // the missing size of the indicator value. As such the buffer length can not be used to
        // correctly index a columnar buffer of Self.
        self.buffer.borrow().len().try_into().unwrap()
    }
}

impl<B, K> HasDataType for VarCell<B, K>
where
    B: Borrow<[u8]>,
    K: VarKind,
{
    fn data_type(&self) -> DataType {
        K::relational_type(self.buffer.borrow().len())
    }
}

unsafe impl<B, K> CDataMut for VarCell<B, K>
where
    B: BorrowMut<[u8]>,
    K: VarKind,
{
    fn mut_indicator_ptr(&mut self) -> *mut isize {
        &mut self.indicator as *mut isize
    }

    fn mut_value_ptr(&mut self) -> *mut c_void {
        self.buffer.borrow_mut().as_mut_ptr() as *mut c_void
    }
}

/// Binds a byte array as a VarChar input parameter.
///
/// While a byte array can provide us with a pointer to the start of the array and the length of the
/// array itself, it can not provide us with a pointer to the length of the buffer. So to bind
/// strings which are not zero terminated we need to store the length in a separate value.
///
/// This type is created if `into_parameter` of the `IntoParameter` trait is called on a `&str`.
///
/// # Example
///
/// ```no_run
/// use odbc_api::{Environment, IntoParameter};
///
/// let env = Environment::new()?;
///
/// let mut conn = env.connect("YourDatabase", "SA", "My@Test@Password1")?;
/// if let Some(cursor) = conn.execute(
///     "SELECT year FROM Birthdays WHERE name=?;",
///     &"Bernd".into_parameter())?
/// {
///     // Use cursor to process query results.
/// };
/// # Ok::<(), odbc_api::Error>(())
/// ```
pub type VarCharSlice<'a> = VarChar<&'a [u8]>;

/// Binds a byte array as a variadic binary input parameter.
///
/// While a byte array can provide us with a pointer to the start of the array and the length of the
/// array itself, it can not provide us with a pointer to the length of the buffer. So to bind
/// byte slices (`&[u8]`) we need to store the length in a separate value.
///
/// This type is created if `into_parameter` of the `IntoParameter` trait is called on a `&[u8]`.
pub type VarBinarySlice<'a> = VarBinary<&'a [u8]>;

impl<'a, K> VarCell<&'a [u8], K>
where
    K: VarKind,
{
    /// Indicates missing data
    pub const NULL: Self = Self {
        // We do not want to use the empty buffer (`&[]`) here. It would be bound as `VARCHAR(0)`
        // which caused errors with Microsoft Access and older versions of the Microsoft SQL Server
        // ODBC driver.
        buffer: &[0],
        indicator: NULL_DATA,
        kind: PhantomData,
    };

    /// Constructs a new VarChar containing the text in the specified buffer.
    ///
    /// Caveat: This constructor is going to create a truncated value in case the input slice ends
    /// with `nul`. Should you want to insert an actual string those payload ends with `nul` into
    /// the database you need a buffer one byte longer than the string. You can instantiate such a
    /// value using [`Self::from_buffer`].
    pub fn new(value: &'a [u8]) -> Self {
        Self::from_buffer(value, Indicator::Length(value.len()))
    }
}

/// Wraps a slice so it can be used as an output parameter for character data.
pub type VarCharSliceMut<'a> = VarChar<&'a mut [u8]>;

/// Wraps a slice so it can be used as an output parameter for binary data.
pub type VarBinarySliceMut<'a> = VarBinary<&'a mut [u8]>;

/// A stack allocated VARCHAR type.
///
/// Due to its memory layout this type can be bound either as a single parameter, or as a column of
/// a row-by-row output, but not be used in columnar parameter arrays or output buffers.
pub type VarCharArray<const LENGTH: usize> = VarChar<[u8; LENGTH]>;

/// A stack allocated VARBINARY type.
///
/// Due to its memory layout this type can be bound either as a single parameter, or as a column of
/// a row-by-row output, but not be used in columnar parameter arrays or output buffers.
pub type VarBinaryArray<const LENGTH: usize> = VarBinary<[u8; LENGTH]>;

impl<const LENGTH: usize, K: VarKind> VarCell<[u8; LENGTH], K> {
    /// Indicates a missing value.
    pub const NULL: Self = Self {
        buffer: [0; LENGTH],
        indicator: NULL_DATA,
        kind: PhantomData,
    };

    /// Construct from a slice. If value is longer than `LENGTH` it will be truncated. In that case
    /// the last byte will be set to `0`.
    pub fn new(bytes: &[u8]) -> Self {
        let indicator = bytes.len().try_into().unwrap();
        let mut buffer = [0u8; LENGTH];
        if bytes.len() > LENGTH {
            buffer.copy_from_slice(&bytes[..LENGTH]);
            *buffer.last_mut().unwrap() = 0;
        } else {
            buffer[..bytes.len()].copy_from_slice(bytes);
        };
        Self {
            buffer,
            indicator,
            kind: PhantomData,
        }
    }
}

/// Figures out, wether or not the buffer ends with a fixed number of zeroes.
fn ends_in_zeroes(buffer: &[u8], number_of_zeroes: usize) -> bool {
    buffer.len() >= number_of_zeroes
        && buffer
            .iter()
            .rev()
            .copied()
            .take(number_of_zeroes)
            .all(|byte| byte == 0)
}

// We can't go all out and implement these traits for anything implementing Borrow and BorrowMut,
// because erroneous but still safe implementation of these traits could cause invalid memory access
// down the road. E.g. think about returning a different slice with a different length for borrow
// and borrow_mut.
unsafe impl<K: VarKind> CElement for VarCell<&'_ [u8], K> {}

unsafe impl<const LENGTH: usize, K: VarKind> CElement for VarCell<[u8; LENGTH], K> {}
unsafe impl<const LENGTH: usize, K: VarKind> OutputParameter for VarCell<[u8; LENGTH], K> {}

unsafe impl<K: VarKind> CElement for VarCell<&'_ mut [u8], K> {}
unsafe impl<K: VarKind> OutputParameter for VarCell<&'_ mut [u8], K> {}

unsafe impl<K: VarKind> CElement for VarCell<Box<[u8]>, K> {}
unsafe impl<K: VarKind> OutputParameter for VarCell<Box<[u8]>, K> {}

#[cfg(test)]
mod tests {

    use super::{Indicator, VarCharSlice};

    #[test]
    fn must_accept_fitting_values_and_correctly_truncated_ones() {
        // Fine: not truncated
        VarCharSlice::from_buffer(b"12345", Indicator::Length(5));
        // Fine: truncated, but ends in zero
        VarCharSlice::from_buffer(b"1234\0", Indicator::Length(10));
    }

    #[test]
    #[should_panic]
    fn must_ensure_truncated_values_are_terminated() {
        // Not fine, value is too long, but not terminated by zero
        VarCharSlice::from_buffer(b"12345", Indicator::Length(10));
    }
}
