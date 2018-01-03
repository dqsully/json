// Copyright 2017 Serde Developers
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Deserialize JSON data to a Rust data structure.

use std::{i32, u64};
use std::io;
use std::marker::PhantomData;

use serde::de::{self, Expected, Unexpected};

use super::error::{Error, ErrorCode, Result};

use read::{self, Reference};

pub use read::{Read, IoRead, SliceRead, StrRead};

//////////////////////////////////////////////////////////////////////////////

/// A structure that deserializes JSON into Rust values.
pub struct Deserializer<R> {
    read: R,
    str_buf: Vec<u8>,
    remaining_depth: u8,
    capture: bool,
}

impl<'de, R> Deserializer<R>
where
    R: read::Read<'de>,
{
    /// Create a JSON deserializer from one of the possible serde_json input
    /// sources.
    ///
    /// Typically it is more convenient to use one of these methods instead:
    ///
    ///   - Deserializer::from_str
    ///   - Deserializer::from_bytes
    ///   - Deserializer::from_reader
    pub fn new(read: R) -> Self {
        Deserializer {
            read: read,
            str_buf: Vec::with_capacity(128),
            remaining_depth: 128,
            capture: false,
        }
    }
}

impl<R> Deserializer<read::IoRead<R>>
where
    R: io::Read,
{
    /// Creates a JSON deserializer from an `io::Read`.
    pub fn from_reader(reader: R) -> Self {
        Deserializer::new(read::IoRead::new(reader))
    }
}

impl<'a> Deserializer<read::SliceRead<'a>> {
    /// Creates a JSON deserializer from a `&[u8]`.
    pub fn from_slice(bytes: &'a [u8]) -> Self {
        Deserializer::new(read::SliceRead::new(bytes))
    }
}

impl<'a> Deserializer<read::StrRead<'a>> {
    /// Creates a JSON deserializer from a `&str`.
    pub fn from_str(s: &'a str) -> Self {
        Deserializer::new(read::StrRead::new(s))
    }
}

macro_rules! overflow {
    ($a:ident * 10 + $b:ident, $c:expr) => {
        $a >= $c / 10 && ($a > $c / 10 || $b > $c % 10)
    }
}

enum Number {
    F64(f64),
    U64(u64),
    I64(i64),
}

impl Number {
    fn visit<'de, V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        match self {
            Number::F64(x) => visitor.visit_f64(x),
            Number::U64(x) => visitor.visit_u64(x),
            Number::I64(x) => visitor.visit_i64(x),
        }
    }

    fn invalid_type(self, exp: &Expected) -> Error {
        match self {
            Number::F64(x) => de::Error::invalid_type(Unexpected::Float(x), exp),
            Number::U64(x) => de::Error::invalid_type(Unexpected::Unsigned(x), exp),
            Number::I64(x) => de::Error::invalid_type(Unexpected::Signed(x), exp),
        }
    }
}

impl<'de, R: Read<'de>> Deserializer<R> {
    /// The `Deserializer::end` method should be called after a value has been fully deserialized.
    /// This allows the `Deserializer` to validate that the input stream is at the end or that it
    /// only has trailing whitespace.
    pub fn end(&mut self) -> Result<()> {
        match try!(self.parse_whitespace()) {
            Some(_) => Err(self.peek_error(ErrorCode::TrailingCharacters)),
            None => Ok(()),
        }
    }

    /// Turn a JSON deserializer into an iterator over values of type T.
    pub fn into_iter<T>(self) -> StreamDeserializer<'de, R, T>
    where
        T: de::Deserialize<'de>,
    {
        // This cannot be an implementation of std::iter::IntoIterator because
        // we need the caller to choose what T is.
        let offset = self.read.byte_offset();
        StreamDeserializer {
            de: self,
            offset: offset,
            output: PhantomData,
            lifetime: PhantomData,
        }
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        self.read.peek().map_err(Error::io)
    }

    fn peek_or_null(&mut self) -> Result<u8> {
        Ok(try!(self.peek()).unwrap_or(b'\x00'))
    }

    fn eat_char(&mut self) {
        if let Ok(Some(c)) = self.next_char() {
            if self.capture {
                self.str_buf.push(c);
            }
        }
    }

    fn next_char(&mut self) -> Result<Option<u8>> {
        let ret = self.read.next().map_err(Error::io);

        if let Ok(Some(c)) = ret {
            if self.capture {
                self.str_buf.push(c);
            }
        }

        ret
    }

    fn next_char_or_null(&mut self) -> Result<u8> {
        let ret = Ok(try!(self.next_char()).unwrap_or(b'\x00'));

        if let Ok(c) = ret {
            if self.capture {
                self.str_buf.push(c);
            }
        }

        ret
    }

    /// Error caused by a byte from next_char().
    #[cold]
    fn error(&self, reason: ErrorCode) -> Error {
        let pos = self.read.position();
        Error::syntax(reason, pos.line, pos.column)
    }

    /// Error caused by a byte from peek().
    #[cold]
    fn peek_error(&self, reason: ErrorCode) -> Error {
        let pos = self.read.peek_position();
        Error::syntax(reason, pos.line, pos.column)
    }

    /// Returns the first non-whitespace byte without consuming it, or `None` if
    /// EOF is encountered.
    fn parse_whitespace(&mut self) -> Result<Option<u8>> {
        debug!(parse_whitespace);
        let mut line_comment = false;
        let mut multiline_comment = false;

        loop {
            if line_comment {
                match try!(self.peek()) {
                    Some(b'\n') | Some(b'\r') => {
                        line_comment = false;
                    }
                    Some(_) => {}
                    None => {
                        return Ok(None);
                    }
                }

                self.eat_char();
            } else if multiline_comment {
                match try!(self.peek()) {
                    Some(b'*') => {
                        self.eat_char();

                        if let Some(b'/') = try!(self.peek()) {
                                multiline_comment = false;
                        }
                    }
                    Some(_) => {}
                    None => {
                        return Ok(None);
                    }
                }

                self.eat_char();
            } else {
                match try!(self.peek()) {
                    Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'\r') => {}
                    Some(b'#') => {
                        line_comment = true;
                    }
                    Some(b'/') => {
                        self.eat_char();

                        match try!(self.peek()) {
                            Some(b'/') => {
                                line_comment = true;
                            }
                            Some(b'*') => {
                                multiline_comment = true;
                            }
                            other => {
                                return Ok(other);
                            }
                        }
                    }
                    other => {
                        return Ok(other);
                    }
                }

                self.eat_char();
            }
        }
    }

    /// Returns the first non-whitespace byte without consuming it, or `None` if
    /// EOF is encountered
    /// Also returns whether the parsing traversed a newline character through a
    /// reference parameter
    fn parse_whitespace_get_newline(&mut self, had_newline: &mut bool) -> Result<Option<u8>> {
        debug!(parse_whitespace_get_newline);
        let mut line_comment = false;
        let mut multiline_comment = false;

        loop {
            if line_comment {
                match try!(self.peek()) {
                    Some(b'\n') | Some(b'\r') => {
                        line_comment = false;
                    }
                    Some(_) => {}
                    None => {
                        return Ok(None);
                    }
                }

                self.eat_char();
            } else if multiline_comment {
                match try!(self.peek()) {
                    Some(b'*') => {
                        if let Some(b'/') = try!(self.peek()) {
                                multiline_comment = false;
                                self.eat_char();
                        }
                    }
                    Some(_) => {}
                    None => {
                        return Ok(None);
                    }
                }

                self.eat_char();
            } else {
                match try!(self.peek()) {
                    Some(b' ') | Some(b'\t') => {},
                    Some(b'\n') | Some(b'\r') => {
                        *had_newline = true;
                    }
                    Some(b'#') => {
                        line_comment = true;
                    }
                    Some(b'/') => {
                        match try!(self.peek()) {
                            Some(b'/') => {
                                line_comment = true;
                            }
                            Some(b'*') => {
                                multiline_comment = true;
                            }
                            other => {
                                return Ok(other);
                            }
                        }

                        self.eat_char();
                    }
                    other => {
                        return Ok(other);
                    }
                }

                self.eat_char();
            }
        }
    }

    /// Returns the first non-whitespace byte without consuming it, or `None` if
    /// EOF is encountered
    /// Also returns whether the parsing traversed a newline character through a
    /// reference parameter
    fn parse_whitespace_until_newline(&mut self, had_newline: &mut bool) -> Result<Option<u8>> {
        debug!(parse_whitespace_until_newline);
        let mut line_comment = false;
        let mut multiline_comment = false;

        let mut prev = None;

        loop {
            if line_comment {
                match try!(self.peek()) {
                    Some(b'\n') | Some(b'\r') => {
                        *had_newline = true;
                        return Ok(prev);
                    }
                    Some(_) => {}
                    None => {
                        return Ok(None);
                    }
                }

                self.eat_char();
            } else if multiline_comment {
                match try!(self.peek()) {
                    Some(b'*') => {
                        self.eat_char();

                        if let Some(b'/') = try!(self.peek()) {
                                multiline_comment = false;
                        }
                    }
                    Some(_) => {}
                    None => {
                        return Ok(None);
                    }
                }

                self.eat_char();
            } else {
                match try!(self.peek()) {
                    Some(b' ') | Some(b'\t') => {},
                    Some(b'\n') | Some(b'\r') => {
                        *had_newline = true;
                        return Ok(prev);
                    }
                    Some(b'#') => {
                        line_comment = true;
                    }
                    Some(b'/') => {
                        match try!(self.peek()) {
                            Some(b'/') => {
                                line_comment = true;
                            }
                            Some(b'*') => {
                                multiline_comment = true;
                            }
                            other => {
                                return Ok(other);
                            }
                        }

                        self.eat_char();
                    }
                    other => {
                        return Ok(other);
                    }
                }

                self.eat_char();
            }

            prev = try!(self.peek());
        }
    }

    #[cold]
    fn peek_invalid_type(&mut self, exp: &Expected) -> Error {
        debug!(peek_invalid_type);
        let err = match self.peek_or_null().unwrap_or(b'\x00') {
            b'n' => {
                self.eat_char();
                if let Err(err) = self.parse_ident(b"ull") {
                    return err;
                }
                de::Error::invalid_type(Unexpected::Unit, exp)
            }
            b't' => {
                self.eat_char();
                if let Err(err) = self.parse_ident(b"rue") {
                    return err;
                }
                de::Error::invalid_type(Unexpected::Bool(true), exp)
            }
            b'f' => {
                self.eat_char();
                if let Err(err) = self.parse_ident(b"alse") {
                    return err;
                }
                de::Error::invalid_type(Unexpected::Bool(false), exp)
            }
            b'-' => {
                self.eat_char();
                match self.parse_integer(false) {
                    Ok(n) => n.invalid_type(exp),
                    Err(err) => return err,
                }
            }
            b'0'...b'9' => {
                match self.parse_integer(true) {
                    Ok(n) => n.invalid_type(exp),
                    Err(err) => return err,
                }
            }
            b'"' => {
                self.eat_char();
                self.str_buf.clear();
                match self.read.parse_double_str(&mut self.str_buf) {
                    Ok(s) => de::Error::invalid_type(Unexpected::Str(&s), exp),
                    Err(err) => return err,
                }
            }
            b'\'' => {
                self.eat_char();
                self.str_buf.clear();
                match self.read.parse_single_str(&mut self.str_buf) {
                    Ok(s) => de::Error::invalid_type(Unexpected::Str(&s), exp),
                    Err(err) => return err,
                }
            }
            b'[' => {
                de::Error::invalid_type(Unexpected::Seq, exp)
            }
            b'{' => {
                de::Error::invalid_type(Unexpected::Map, exp)
            }
            _ => self.peek_error(ErrorCode::ExpectedSomeValue),
        };

        self.fix_position(err)
    }

    fn deserialize_number<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_number);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'-' => {
                self.eat_char();
                try!(self.parse_integer(false)).visit(visitor)
            }
            b'0'...b'9' => try!(self.parse_integer(true)).visit(visitor),
            _ => {
                self.str_buf.clear();
                match try!(self.read.parse_none_str(&mut self.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    #[cold]
    fn fix_position(&self, err: Error) -> Error {
        err.fix_position(move |code| self.error(code))
    }

    fn parse_ident(&mut self, ident: &[u8]) -> Result<()> {
        debug!(parse_ident);
        for c in ident {
            if Some(*c) != try!(self.next_char()) {
                return Err(self.error(ErrorCode::ExpectedSomeIdent));
            }
        }

        // Consume until newline, comma, or eof
        let mut had_newline = false;

        let c = try!(self.parse_whitespace_until_newline(&mut had_newline));

        if had_newline {
            Ok(())
        } else {
            match c {
                Some(b',') |
                Some(b']') |
                Some(b'}') => Ok(()),
                _ => Err(self.error(ErrorCode::UnexpectedCharacter)),
            }
        }
    }

    fn parse_integer(&mut self, pos: bool) -> Result<Number> {
        debug!(parse_integer);
        match try!(self.peek_or_null()) {
            b'0' => {
                try!(self.next_char());
                // There can be only one leading '0'.
                match try!(self.peek_or_null()) {
                    b'0'...b'9' => Err(self.peek_error(ErrorCode::InvalidNumber)),
                    _ => self.parse_number(pos, 0),
                }
            }
            c @ b'1'...b'9' => {
                try!(self.next_char());
                let mut res = (c - b'0') as u64;

                loop {
                    match try!(self.peek_or_null()) {
                        c @ b'0'...b'9' => {
                            self.eat_char();
                            let digit = (c - b'0') as u64;

                            // We need to be careful with overflow. If we can, try to keep the
                            // number as a `u64` until we grow too large. At that point, switch to
                            // parsing the value as a `f64`.
                            if overflow!(res * 10 + digit, u64::MAX) {
                                return Ok(Number::F64(try!(self.parse_long_integer(
                                    pos,
                                    res,
                                    1, // res * 10^1
                                ))));
                            }

                            res = res * 10 + digit;
                        }
                        _ => {
                            return self.parse_number(pos, res);
                        }
                    }
                }
            }
            _ => Err(self.error(ErrorCode::InvalidNumber)),
        }
    }

    fn parse_long_integer(
        &mut self,
        pos: bool,
        significand: u64,
        mut exponent: i32,
    ) -> Result<f64> {
        debug!(parse_long_integer);
        loop {
            match try!(self.peek_or_null()) {
                b'0'...b'9' => {
                    self.eat_char();
                    // This could overflow... if your integer is gigabytes long.
                    // Ignore that possibility.
                    exponent += 1;
                }
                b'.' => {
                    return self.parse_decimal(pos, significand, exponent);
                }
                b'e' | b'E' => {
                    return self.parse_exponent(pos, significand, exponent);
                }
                _ => {
                    return self.f64_from_parts(pos, significand, exponent);
                }
            }
        }
    }

    fn parse_number(&mut self, pos: bool, significand: u64) -> Result<Number> {
        debug!(parse_number);
        let ret = Ok(match try!(self.peek_or_null()) {
            b'.' => Number::F64(try!(self.parse_decimal(pos, significand, 0))),
            b'e' | b'E' => Number::F64(try!(self.parse_exponent(pos, significand, 0))),
            _ => {
                if pos {
                    Number::U64(significand)
                } else {
                    let neg = (significand as i64).wrapping_neg();

                    // Convert into a float if we underflow.
                    if neg > 0 {
                        Number::F64(-(significand as f64))
                    } else {
                        Number::I64(neg)
                    }
                }
            }
        });

        // Consume until newline, comma, or eof
        let mut had_newline = false;

        let c = try!(self.parse_whitespace_until_newline(&mut had_newline));

        if had_newline {
            ret
        } else {
            match c {
                Some(b',') |
                Some(b']') |
                Some(b'}') => ret,
                _ => Err(self.error(ErrorCode::UnexpectedCharacter)),
            }
        }
    }

    fn parse_decimal(
        &mut self,
        pos: bool,
        mut significand: u64,
        mut exponent: i32,
    ) -> Result<f64> {
        debug!(parse_decimal);
        self.eat_char();

        let mut at_least_one_digit = false;
        while let c @ b'0'...b'9' = try!(self.peek_or_null()) {
            self.eat_char();
            let digit = (c - b'0') as u64;
            at_least_one_digit = true;

            if overflow!(significand * 10 + digit, u64::MAX) {
                // The next multiply/add would overflow, so just ignore all
                // further digits.
                while let b'0'...b'9' = try!(self.peek_or_null()) {
                    self.eat_char();
                }
                break;
            }

            significand = significand * 10 + digit;
            exponent -= 1;
        }

        if !at_least_one_digit {
            return Err(self.peek_error(ErrorCode::InvalidNumber));
        }

        match try!(self.peek_or_null()) {
            b'e' | b'E' => self.parse_exponent(pos, significand, exponent),
            _ => self.f64_from_parts(pos, significand, exponent),
        }
    }

    fn parse_exponent(
        &mut self,
        pos: bool,
        significand: u64,
        starting_exp: i32,
    ) -> Result<f64> {
        debug!(parse_exponent);
        self.eat_char();

        let pos_exp = match try!(self.peek_or_null()) {
            b'+' => {
                self.eat_char();
                true
            }
            b'-' => {
                self.eat_char();
                false
            }
            _ => true,
        };

        // Make sure a digit follows the exponent place.
        let mut exp = match try!(self.next_char_or_null()) {
            c @ b'0'...b'9' => (c - b'0') as i32,
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        };

        while let c @ b'0'...b'9' = try!(self.peek_or_null()) {
            self.eat_char();
            let digit = (c - b'0') as i32;

            if overflow!(exp * 10 + digit, i32::MAX) {
                return self.parse_exponent_overflow(pos, significand, pos_exp);
            }

            exp = exp * 10 + digit;
        }

        let final_exp = if pos_exp {
            starting_exp.saturating_add(exp)
        } else {
            starting_exp.saturating_sub(exp)
        };

        self.f64_from_parts(pos, significand, final_exp)
    }

    // This cold code should not be inlined into the middle of the hot
    // exponent-parsing loop above.
    #[cold]
    #[inline(never)]
    fn parse_exponent_overflow(
        &mut self,
        pos: bool,
        significand: u64,
        pos_exp: bool,
    ) -> Result<f64> {
        debug!(parse_exponent_overflow);
        // Error instead of +/- infinity.
        if significand != 0 && pos_exp {
            return Err(self.error(ErrorCode::NumberOutOfRange));
        }

        while let b'0'...b'9' = try!(self.peek_or_null()) {
            self.eat_char();
        }
        Ok(if pos { 0.0 } else { -0.0 })
    }

    fn f64_from_parts(
        &mut self,
        pos: bool,
        significand: u64,
        mut exponent: i32,
    ) -> Result<f64> {
        debug!(f64_from_parts);
        let mut f = significand as f64;
        loop {
            match POW10.get(exponent.abs() as usize) {
                Some(&pow) => {
                    if exponent >= 0 {
                        f *= pow;
                        if f.is_infinite() {
                            return Err(self.error(ErrorCode::NumberOutOfRange));
                        }
                    } else {
                        f /= pow;
                    }
                    break;
                }
                None => {
                    if f == 0.0 {
                        break;
                    }
                    if exponent >= 0 {
                        return Err(self.error(ErrorCode::NumberOutOfRange));
                    }
                    f /= 1e308;
                    exponent += 308;
                }
            }
        }
        Ok(if pos { f } else { -f })
    }

    fn parse_object_colon(&mut self) -> Result<()> {
        debug!(parse_object_colon);
        match try!(self.parse_whitespace()) {
            Some(b':') => {
                self.eat_char();
                Ok(())
            }
            Some(_) => Err(self.peek_error(ErrorCode::ExpectedColon)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingObject)),
        }
    }

    fn end_seq(&mut self) -> Result<()> {
        debug!(end_seq);
        let ch = try!(self.parse_whitespace());
        match ch {
            Some(b']') => {
                self.eat_char();
                Ok(())
            }
            Some(b',') => {
                self.eat_char();
                match self.parse_whitespace() {
                    Ok(Some(b']')) => Err(self.peek_error(ErrorCode::ExtraComma)),
                    _ => Err(self.peek_error(ErrorCode::TrailingCharacters)), // This shouldn't be possible
                }
            }
            Some(_) => Err(self.peek_error(ErrorCode::TrailingCharacters)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingList)),
        }
    }

    fn end_map(&mut self) -> Result<()> {
        debug!(end_map);
        match try!(self.parse_whitespace()) {
            Some(b'}') => {
                self.eat_char();
                Ok(())
            }
            Some(b',') => Err(self.peek_error(ErrorCode::ExtraComma)),
            Some(_) => Err(self.peek_error(ErrorCode::TrailingCharacters)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingObject)),
        }
    }

    fn ignore_value(&mut self) -> Result<()> {
        debug!(ignore_value);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        match peek {
            b'n' => {
                self.eat_char();
                self.parse_ident(b"ull")
            }
            b't' => {
                self.eat_char();
                self.parse_ident(b"rue")
            }
            b'f' => {
                self.eat_char();
                self.parse_ident(b"alse")
            }
            b'-' => {
                self.eat_char();
                self.ignore_integer()
            }
            b'0'...b'9' => {
                self.ignore_integer()
            }
            b'"' => {
                self.eat_char();
                self.read.ignore_double_str()
            }
            b'\'' => {
                self.eat_char();
                self.read.ignore_single_str()
            }
            b'[' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let res = self.ignore_seq();
                self.remaining_depth += 1;
                res
            }
            b'{' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let res = self.ignore_map();
                self.remaining_depth += 1;
                res
            }
            _ => {
                self.read.ignore_none_str()
            }
        }
    }

    fn ignore_integer(&mut self) -> Result<()> {
        debug!(ignore_integer);
        match try!(self.next_char_or_null()) {
            b'0' => {
                // There can be only one leading '0'.
                if let b'0'...b'9' = try!(self.peek_or_null()) {
                    return Err(self.peek_error(ErrorCode::InvalidNumber));
                }
            }
            b'1'...b'9' => {
                while let b'0'...b'9' = try!(self.peek_or_null()) {
                    self.eat_char();
                }
            }
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        }

        match try!(self.peek_or_null()) {
            b'.' => self.ignore_decimal(),
            b'e' | b'E' => self.ignore_exponent(),
            _ => Ok(()),
        }
    }

    fn ignore_decimal(&mut self) -> Result<()> {
        debug!(ignore_decimal);
        self.eat_char();

        let mut at_least_one_digit = false;
        while let b'0'...b'9' = try!(self.peek_or_null()) {
            self.eat_char();
            at_least_one_digit = true;
        }

        if !at_least_one_digit {
            return Err(self.peek_error(ErrorCode::InvalidNumber));
        }

        match try!(self.peek_or_null()) {
            b'e' | b'E' => self.ignore_exponent(),
            _ => Ok(()),
        }
    }

    fn ignore_exponent(&mut self) -> Result<()> {
        debug!(ignore_exponent);
        self.eat_char();

        match try!(self.peek_or_null()) {
            b'+' | b'-' => self.eat_char(),
            _ => {}
        }

        // Make sure a digit follows the exponent place.
        match try!(self.next_char_or_null()) {
            b'0'...b'9' => {}
            _ => {
                return Err(self.error(ErrorCode::InvalidNumber));
            }
        }

        while let b'0'...b'9' = try!(self.peek_or_null()) {
            self.eat_char();
        }

        Ok(())
    }

    fn ignore_seq(&mut self) -> Result<()> {
        debug!(ignore_seq);
        let mut had_newline;

        loop {
            match try!(self.parse_whitespace()) {
                Some(b']') => {
                    self.eat_char();
                    return Ok(());
                }
                Some(b',') => {
                    return Err(self.peek_error(ErrorCode::ExtraComma));
                }
                _ => (),
            }

            try!(self.ignore_value());

            had_newline = false;
            match try!(self.parse_whitespace_get_newline(&mut had_newline)) {
                Some(ch) => {
                    if ch == b',' {
                        self.eat_char();
                    } else if ch != b']' && !had_newline {
                        return Err(self.peek_error(ErrorCode::ExpectedListCommaOrEnd));
                    }
                }
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingList));
                }
            }
        }
    }

    fn ignore_map(&mut self) -> Result<()> {
        debug!(ignore_map);
        let mut had_newline;

        loop {
            match try!(self.parse_whitespace()) {
                Some(b'}') => {
                    self.eat_char();
                    return Ok(());
                }
                Some(b',') => {
                    return Err(self.peek_error(ErrorCode::ExtraComma));
                }
                _ => (),
            }

            match try!(self.parse_whitespace()) {
                Some(b'"') => {
                    self.eat_char();
                    try!(self.read.ignore_double_str());
                }
                Some(b'\'') => {
                    self.eat_char();
                    try!(self.read.ignore_single_str());
                }
                Some(_) => {
                    try!(self.read.ignore_member_name());
                }
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingObject));
                }
            }

            match try!(self.parse_whitespace()) {
                Some(b':') => {
                    self.eat_char();
                }
                Some(_) => {
                    return Err(self.peek_error(ErrorCode::ExpectedColon));
                }
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingObject));
                }
            }

            try!(self.ignore_value());

            had_newline = false;
            match try!(self.parse_whitespace_get_newline(&mut had_newline)) {
                Some(ch) => {
                    if ch == b',' {
                        self.eat_char();
                    } else if ch != b'}' && !had_newline {
                        return Err(self.peek_error(ErrorCode::ExpectedListCommaOrEnd));
                    }
                }
                None => {
                    return Err(self.peek_error(ErrorCode::EofWhileParsingList));
                }
            }
        }
    }
}

#[cfg_attr(rustfmt, rustfmt_skip)]
static POW10: [f64; 309] =
    [1e000, 1e001, 1e002, 1e003, 1e004, 1e005, 1e006, 1e007, 1e008, 1e009,
     1e010, 1e011, 1e012, 1e013, 1e014, 1e015, 1e016, 1e017, 1e018, 1e019,
     1e020, 1e021, 1e022, 1e023, 1e024, 1e025, 1e026, 1e027, 1e028, 1e029,
     1e030, 1e031, 1e032, 1e033, 1e034, 1e035, 1e036, 1e037, 1e038, 1e039,
     1e040, 1e041, 1e042, 1e043, 1e044, 1e045, 1e046, 1e047, 1e048, 1e049,
     1e050, 1e051, 1e052, 1e053, 1e054, 1e055, 1e056, 1e057, 1e058, 1e059,
     1e060, 1e061, 1e062, 1e063, 1e064, 1e065, 1e066, 1e067, 1e068, 1e069,
     1e070, 1e071, 1e072, 1e073, 1e074, 1e075, 1e076, 1e077, 1e078, 1e079,
     1e080, 1e081, 1e082, 1e083, 1e084, 1e085, 1e086, 1e087, 1e088, 1e089,
     1e090, 1e091, 1e092, 1e093, 1e094, 1e095, 1e096, 1e097, 1e098, 1e099,
     1e100, 1e101, 1e102, 1e103, 1e104, 1e105, 1e106, 1e107, 1e108, 1e109,
     1e110, 1e111, 1e112, 1e113, 1e114, 1e115, 1e116, 1e117, 1e118, 1e119,
     1e120, 1e121, 1e122, 1e123, 1e124, 1e125, 1e126, 1e127, 1e128, 1e129,
     1e130, 1e131, 1e132, 1e133, 1e134, 1e135, 1e136, 1e137, 1e138, 1e139,
     1e140, 1e141, 1e142, 1e143, 1e144, 1e145, 1e146, 1e147, 1e148, 1e149,
     1e150, 1e151, 1e152, 1e153, 1e154, 1e155, 1e156, 1e157, 1e158, 1e159,
     1e160, 1e161, 1e162, 1e163, 1e164, 1e165, 1e166, 1e167, 1e168, 1e169,
     1e170, 1e171, 1e172, 1e173, 1e174, 1e175, 1e176, 1e177, 1e178, 1e179,
     1e180, 1e181, 1e182, 1e183, 1e184, 1e185, 1e186, 1e187, 1e188, 1e189,
     1e190, 1e191, 1e192, 1e193, 1e194, 1e195, 1e196, 1e197, 1e198, 1e199,
     1e200, 1e201, 1e202, 1e203, 1e204, 1e205, 1e206, 1e207, 1e208, 1e209,
     1e210, 1e211, 1e212, 1e213, 1e214, 1e215, 1e216, 1e217, 1e218, 1e219,
     1e220, 1e221, 1e222, 1e223, 1e224, 1e225, 1e226, 1e227, 1e228, 1e229,
     1e230, 1e231, 1e232, 1e233, 1e234, 1e235, 1e236, 1e237, 1e238, 1e239,
     1e240, 1e241, 1e242, 1e243, 1e244, 1e245, 1e246, 1e247, 1e248, 1e249,
     1e250, 1e251, 1e252, 1e253, 1e254, 1e255, 1e256, 1e257, 1e258, 1e259,
     1e260, 1e261, 1e262, 1e263, 1e264, 1e265, 1e266, 1e267, 1e268, 1e269,
     1e270, 1e271, 1e272, 1e273, 1e274, 1e275, 1e276, 1e277, 1e278, 1e279,
     1e280, 1e281, 1e282, 1e283, 1e284, 1e285, 1e286, 1e287, 1e288, 1e289,
     1e290, 1e291, 1e292, 1e293, 1e294, 1e295, 1e296, 1e297, 1e298, 1e299,
     1e300, 1e301, 1e302, 1e303, 1e304, 1e305, 1e306, 1e307, 1e308];

impl<'de, 'a, R: Read<'de>> de::Deserializer<'de> for &'a mut Deserializer<R> {
    type Error = Error;

    #[inline]
    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_any);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'n' => {
                self.eat_char();
                match self.parse_ident(b"ull") {
                    Ok(_) => visitor.visit_unit(),
                    Err(_) => {
                        self.str_buf.clear();
                        match try!(self.read.parse_none_str(&mut self.str_buf)) {
                            // Can't treat the original string as borrowed anymore
                            Reference::Borrowed(s) => visitor.visit_str(&("null".to_owned() + s)),
                            Reference::Copied(s) => visitor.visit_str(&("null".to_owned() + &s)),
                        }
                    }
                }
            }
            b't' => {
                self.eat_char();
                match self.parse_ident(b"rue") {
                    Ok(_) => visitor.visit_bool(true),
                    Err(_) => {
                        self.str_buf.clear();
                        match try!(self.read.parse_none_str(&mut self.str_buf)) {
                            // Can't treat the original string as borrowed anymore
                            Reference::Borrowed(s) => visitor.visit_str(&("true".to_owned() + s)),
                            Reference::Copied(s) => visitor.visit_str(&("true".to_owned() + &s)),
                        }
                    }
                }
            }
            b'f' => {
                self.eat_char();
                match self.parse_ident(b"alse") {
                    Ok(_) => visitor.visit_bool(false),
                    Err(_) => {
                        self.str_buf.clear();
                        match try!(self.read.parse_none_str(&mut self.str_buf)) {
                            // Can't treat the original string as borrowed anymore
                            Reference::Borrowed(s) => visitor.visit_str(&("false".to_owned() + s)),
                            Reference::Copied(s) => visitor.visit_str(&("false".to_owned() + &s)),
                        }
                    }
                }
            }
            b'-' => {
                self.str_buf.clear();
                self.capture = true;

                self.eat_char();
                let ret = match self.parse_integer(false) {
                    Ok(num) => num.visit(visitor),
                    Err(_) => {
                        let captured_num = unsafe { String::from_utf8_unchecked(self.str_buf.clone()) };

                        self.capture = false;
                        self.str_buf.clear();
                        match try!(self.read.parse_none_str(&mut self.str_buf)) {
                            Reference::Borrowed(s) => visitor.visit_str(&(captured_num + s)),
                            Reference::Copied(s) => visitor.visit_str(&(captured_num + &s)),
                        }
                    }
                };

                self.capture = false;

                ret
            }
            b'0'...b'9' => {
                self.str_buf.clear();
                self.capture = true;

                let ret = match self.parse_integer(true) {
                    Ok(num) => num.visit(visitor),
                    Err(_) => {
                        let captured_num = unsafe { String::from_utf8_unchecked(self.str_buf.clone()) };

                        self.capture = false;
                        self.str_buf.clear();
                        match try!(self.read.parse_none_str(&mut self.str_buf)) {
                            Reference::Borrowed(s) => visitor.visit_str(&(captured_num + s)),
                            Reference::Copied(s) => visitor.visit_str(&(captured_num + &s)),
                        }
                    }
                };

                self.capture = false;

                ret
            }
            b'"' => {
                self.eat_char();
                self.str_buf.clear();
                match try!(self.read.parse_double_str(&mut self.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
            b'\'' => {
                self.eat_char();
                self.str_buf.clear();
                match try!(self.read.parse_single_str(&mut self.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
            b'[' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let ret = visitor.visit_seq(SeqAccess::new(self));

                self.remaining_depth += 1;

                match (ret, self.end_seq()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            b'{' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let ret = visitor.visit_map(MapAccess::new(self));

                self.remaining_depth += 1;

                match (ret, self.end_map()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => {
                self.str_buf.clear();
                match try!(self.read.parse_none_str(&mut self.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
        };

        match value {
            Ok(value) => Ok(value),
            // The de::Error impl creates errors with unknown line and column.
            // Fill in the position here by looking at the current index in the
            // input. There is no way to tell whether this should call `error`
            // or `peek_error` so pick the one that seems correct more often.
            // Worst case, the position is off by one character.
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_bool);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b't' => {
                self.eat_char();
                try!(self.parse_ident(b"rue"));
                visitor.visit_bool(true)
            }
            b'f' => {
                self.eat_char();
                try!(self.parse_ident(b"alse"));
                visitor.visit_bool(false)
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_number(visitor)
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_str);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'"' => {
                self.eat_char();
                self.str_buf.clear();
                match try!(self.read.parse_double_str(&mut self.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
            b'\'' => {
                self.eat_char();
                self.str_buf.clear();
                match try!(self.read.parse_single_str(&mut self.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    /// Parses a JSON string as bytes. Note that this function does not check
    /// whether the bytes represent a valid UTF-8 string.
    ///
    /// The relevant part of the JSON specification is Section 8.2 of [RFC
    /// 7159]:
    ///
    /// > When all the strings represented in a JSON text are composed entirely
    /// > of Unicode characters (however escaped), then that JSON text is
    /// > interoperable in the sense that all software implementations that
    /// > parse it will agree on the contents of names and of string values in
    /// > objects and arrays.
    /// >
    /// > However, the ABNF in this specification allows member names and string
    /// > values to contain bit sequences that cannot encode Unicode characters;
    /// > for example, "\uDEAD" (a single unpaired UTF-16 surrogate). Instances
    /// > of this have been observed, for example, when a library truncates a
    /// > UTF-16 string without checking whether the truncation split a
    /// > surrogate pair.  The behavior of software that receives JSON texts
    /// > containing such values is unpredictable; for example, implementations
    /// > might return different values for the length of a string value or even
    /// > suffer fatal runtime exceptions.
    ///
    /// [RFC 7159]: https://tools.ietf.org/html/rfc7159
    ///
    /// The behavior of serde_json is specified to fail on non-UTF-8 strings
    /// when deserializing into Rust UTF-8 string types such as String, and
    /// succeed with non-UTF-8 bytes when deserializing using this method.
    ///
    /// Escape sequences are processed as usual, and for `\uXXXX` escapes it is
    /// still checked if the hex number represents a valid Unicode code point.
    ///
    /// # Examples
    ///
    /// You can use this to parse JSON strings containing invalid UTF-8 bytes.
    ///
    /// ```rust
    /// extern crate serde_json;
    /// extern crate serde_bytes;
    ///
    /// use serde_bytes::ByteBuf;
    ///
    /// fn look_at_bytes() -> Result<(), serde_json::Error> {
    ///     let json_data = b"\"some bytes: \xe5\x00\xe5\"";
    ///     let bytes: ByteBuf = serde_json::from_slice(json_data)?;
    ///
    ///     assert_eq!(b'\xe5', bytes[12]);
    ///     assert_eq!(b'\0', bytes[13]);
    ///     assert_eq!(b'\xe5', bytes[14]);
    ///
    ///     Ok(())
    /// }
    /// #
    /// # fn main() {
    /// #     look_at_bytes().unwrap();
    /// # }
    /// ```
    ///
    /// Backslash escape sequences like `\n` are still interpreted and required
    /// to be valid, and `\u` escape sequences are required to represent valid
    /// Unicode code points.
    ///
    /// ```rust
    /// extern crate serde_json;
    /// extern crate serde_bytes;
    ///
    /// use serde_bytes::ByteBuf;
    ///
    /// fn look_at_bytes() {
    ///     let json_data = b"\"invalid unicode surrogate: \\uD801\"";
    ///     let parsed: Result<ByteBuf, _> = serde_json::from_slice(json_data);
    ///
    ///     assert!(parsed.is_err());
    ///
    ///     let expected_msg = "unexpected end of hex escape at line 1 column 35";
    ///     assert_eq!(expected_msg, parsed.unwrap_err().to_string());
    /// }
    /// #
    /// # fn main() {
    /// #     look_at_bytes();
    /// # }
    /// ```
    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_bytes);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'"' => {
                self.eat_char();
                self.str_buf.clear();
                match try!(self.read.parse_double_str_raw(&mut self.str_buf)) {
                    Reference::Borrowed(b) => visitor.visit_borrowed_bytes(b),
                    Reference::Copied(b) => visitor.visit_bytes(b),
                }
            }
            b'\'' => {
                self.eat_char();
                self.str_buf.clear();
                match try!(self.read.parse_single_str_raw(&mut self.str_buf)) {
                    Reference::Borrowed(b) => visitor.visit_borrowed_bytes(b),
                    Reference::Copied(b) => visitor.visit_bytes(b),
                }
            }
            b'[' => self.deserialize_seq(visitor),
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    #[inline]
    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_bytes(visitor)
    }

    /// Parses a `null` as a None, and any other values as a `Some(...)`.
    #[inline]
    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_option);
        match try!(self.parse_whitespace()) {
            Some(b'n') => {
                self.eat_char();
                try!(self.parse_ident(b"ull"));
                visitor.visit_none()
            }
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_unit);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'n' => {
                self.eat_char();
                try!(self.parse_ident(b"ull"));
                visitor.visit_unit()
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_unit(visitor)
    }

    /// Parses a newtype struct as the underlying value.
    #[inline]
    fn deserialize_newtype_struct<V>(self, _name: &str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_seq);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'[' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let ret = visitor.visit_seq(SeqAccess::new(self));

                self.remaining_depth += 1;

                match (ret, self.end_seq()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_tuple<V>(
        self,
        _len: usize,
        visitor: V
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_map);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'{' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let ret = visitor.visit_map(MapAccess::new(self));

                self.remaining_depth += 1;

                match (ret, self.end_map()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_struct);
        let peek = match try!(self.parse_whitespace()) {
            Some(b) => b,
            None => {
                return Err(self.peek_error(ErrorCode::EofWhileParsingValue));
            }
        };

        let value = match peek {
            b'[' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let ret = visitor.visit_seq(SeqAccess::new(self));

                self.remaining_depth += 1;

                match (ret, self.end_seq()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            b'{' => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let ret = visitor.visit_map(MapAccess::new(self));

                self.remaining_depth += 1;

                match (ret, self.end_map()) {
                    (Ok(ret), Ok(())) => Ok(ret),
                    (Err(err), _) | (_, Err(err)) => Err(err),
                }
            }
            _ => Err(self.peek_invalid_type(&visitor)),
        };

        match value {
            Ok(value) => Ok(value),
            Err(err) => Err(self.fix_position(err)),
        }
    }

    /// Parses an enum as an object like `{"$KEY":$VALUE}`, where $VALUE is either a straight
    /// value, a `[..]`, or a `{..}`.
    #[inline]
    fn deserialize_enum<V>(
        self,
        _name: &str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_enum);
        match try!(self.parse_whitespace()) {
            Some(b'{') => {
                self.remaining_depth -= 1;
                if self.remaining_depth == 0 {
                    return Err(self.peek_error(ErrorCode::RecursionLimitExceeded));
                }

                self.eat_char();
                let value = try!(visitor.visit_enum(VariantAccess::new(self)));

                self.remaining_depth += 1;

                match try!(self.parse_whitespace()) {
                    Some(b'}') => {
                        self.eat_char();
                        Ok(value)
                    }
                    Some(_) => Err(self.error(ErrorCode::ExpectedSomeValue)),
                    None => Err(self.error(ErrorCode::EofWhileParsingObject)),
                }
            }
            Some(b'"') => visitor.visit_enum(UnitVariantAccess::new(self)),
            Some(_) => Err(self.peek_error(ErrorCode::ExpectedSomeValue)),
            None => Err(self.peek_error(ErrorCode::EofWhileParsingValue)),
        }
    }

    fn deserialize_identifier<V>(
        self,
        visitor: V
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_ignored_any);
        try!(self.ignore_value());
        visitor.visit_unit()
    }
}

struct SeqAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

impl<'a, R: 'a> SeqAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        SeqAccess {
            de: de,
        }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::SeqAccess<'de> for SeqAccess<'a, R> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
    where
        T: de::DeserializeSeed<'de>,
    {
        debug!(next_element_seed);
        match try!(self.de.parse_whitespace()) {
            Some(b']') => {
                return Ok(None);
            }
            Some(b',') => {
                return Err(self.de.peek_error(ErrorCode::ExtraComma));
            }
            _ => (),
        }

        let ret = seed.deserialize(&mut *self.de).map(Some);

        let mut had_newline = false;
        match try!(self.de.parse_whitespace_get_newline(&mut had_newline)) {
            Some(ch) => {
                if ch == b',' {
                    self.de.eat_char();
                } else if ch != b']' && !had_newline {
                    return Err(self.de.peek_error(ErrorCode::ExpectedListCommaOrEnd));
                }
            }
            None => {
                return Err(self.de.peek_error(ErrorCode::EofWhileParsingList));
            }
        }

        ret
    }
}

struct MapAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

impl<'a, R: 'a> MapAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        MapAccess {
            de: de,
        }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::MapAccess<'de> for MapAccess<'a, R> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
    where
        K: de::DeserializeSeed<'de>,
    {
        debug!(next_key_seed);
        match try!(self.de.parse_whitespace()) {
            Some(b'}') => {
                return Ok(None);
            }
            Some(b',') => {
                return Err(self.de.peek_error(ErrorCode::ExtraComma));
            },
            Some(_) => {
                seed.deserialize(MapKey { de: &mut *self.de }).map(Some)
            }
            None => {
                Err(self.de.peek_error(ErrorCode::EofWhileParsingObject))
            }
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
    where
        V: de::DeserializeSeed<'de>,
    {
        debug!(next_value_seed);
        try!(self.de.parse_object_colon());

        let ret = seed.deserialize(&mut *self.de);

        let mut had_newline = false;
        match try!(self.de.parse_whitespace_get_newline(&mut had_newline)) {
            Some(ch) => {
                if ch == b',' {
                    self.de.eat_char();
                } else if ch != b'}' && !had_newline {
                    return Err(self.de.peek_error(ErrorCode::ExpectedListCommaOrEnd));
                }
            }
            None => {
                return Err(self.de.peek_error(ErrorCode::EofWhileParsingList));
            }
        }

        ret
    }
}

struct VariantAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

impl<'a, R: 'a> VariantAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        VariantAccess { de: de }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::EnumAccess<'de> for VariantAccess<'a, R> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self)>
    where
        V: de::DeserializeSeed<'de>,
    {
        debug!(variant_seed);
        let val = try!(seed.deserialize(&mut *self.de));
        try!(self.de.parse_object_colon());
        Ok((val, self))
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::VariantAccess<'de> for VariantAccess<'a, R> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        de::Deserialize::deserialize(self.de)
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value>
    where
        T: de::DeserializeSeed<'de>,
    {
        debug!(newtype_variant_seed);
        seed.deserialize(self.de)
    }

    fn tuple_variant<V>(self, _len: usize, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(tuple_variant);
        de::Deserializer::deserialize_seq(self.de, visitor)
    }

    fn struct_variant<V>(self, fields: &'static [&'static str], visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(struct_variant);
        de::Deserializer::deserialize_struct(self.de, "", fields, visitor)
    }
}

struct UnitVariantAccess<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

impl<'a, R: 'a> UnitVariantAccess<'a, R> {
    fn new(de: &'a mut Deserializer<R>) -> Self {
        UnitVariantAccess { de: de }
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::EnumAccess<'de> for UnitVariantAccess<'a, R> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self)>
    where
        V: de::DeserializeSeed<'de>,
    {
        debug!(variant_seed);
        let variant = try!(seed.deserialize(&mut *self.de));
        Ok((variant, self))
    }
}

impl<'de, 'a, R: Read<'de> + 'a> de::VariantAccess<'de> for UnitVariantAccess<'a, R> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value>
    where
        T: de::DeserializeSeed<'de>,
    {
        Err(de::Error::invalid_type(Unexpected::UnitVariant, &"newtype variant"),)
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(de::Error::invalid_type(Unexpected::UnitVariant, &"tuple variant"),)
    }

    fn struct_variant<V>(self, _fields: &'static [&'static str], _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(de::Error::invalid_type(Unexpected::UnitVariant, &"struct variant"),)
    }
}

/// Only deserialize from this after peeking a '"' byte! Otherwise it may
/// deserialize invalid JSON successfully.
struct MapKey<'a, R: 'a> {
    de: &'a mut Deserializer<R>,
}

macro_rules! deserialize_integer_key {
    ($deserialize:ident => $visit:ident) => {
        fn $deserialize<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            debug!($deserialize);
            let string;
            match try!(self.de.peek()) {
                Some(b'"') => {
                    self.de.eat_char();
                    self.de.str_buf.clear();
                    string = try!(self.de.read.parse_double_str(&mut self.de.str_buf))
                }
                Some(b'\'') => {
                    self.de.eat_char();
                    self.de.str_buf.clear();
                    string = try!(self.de.read.parse_single_str(&mut self.de.str_buf))
                }
                Some(_) => {
                    self.de.str_buf.clear();
                    string = try!(self.de.read.parse_none_str(&mut self.de.str_buf));
                }
                None => {
                    return Err(self.de.peek_error(ErrorCode::EofWhileParsingObject));
                }
            }

            match (string.parse(), string) {
                (Ok(integer), _) => visitor.$visit(integer),
                (Err(_), Reference::Borrowed(s)) => visitor.visit_borrowed_str(s),
                (Err(_), Reference::Copied(s)) => visitor.visit_str(s),
            }
        }
    }
}

impl<'de, 'a, R> de::Deserializer<'de> for MapKey<'a, R>
where
    R: Read<'de>,
{
    type Error = Error;

    #[inline]
    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        debug!(deserialize_any);
        match try!(self.de.peek_or_null()) {
            b'"' => {
                self.de.eat_char();
                self.de.str_buf.clear();
                match try!(self.de.read.parse_double_str(&mut self.de.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            },
            b'\'' => {
                self.de.eat_char();
                self.de.str_buf.clear();
                match try!(self.de.read.parse_single_str(&mut self.de.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            },
            _ => {
                self.de.str_buf.clear();
                match try!(self.de.read.parse_member_name(&mut self.de.str_buf)) {
                    Reference::Borrowed(s) => visitor.visit_borrowed_str(s),
                    Reference::Copied(s) => visitor.visit_str(s),
                }
            },
        }
    }

    deserialize_integer_key!(deserialize_i8 => visit_i8);
    deserialize_integer_key!(deserialize_i16 => visit_i16);
    deserialize_integer_key!(deserialize_i32 => visit_i32);
    deserialize_integer_key!(deserialize_i64 => visit_i64);
    deserialize_integer_key!(deserialize_u8 => visit_u8);
    deserialize_integer_key!(deserialize_u16 => visit_u16);
    deserialize_integer_key!(deserialize_u32 => visit_u32);
    deserialize_integer_key!(deserialize_u64 => visit_u64);

    #[inline]
    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        // Map keys cannot be null.
        visitor.visit_some(self)
    }

    #[inline]
    fn deserialize_newtype_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    #[inline]
    fn deserialize_enum<V>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.deserialize_enum(name, variants, visitor)
    }

    #[inline]
    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.deserialize_bytes(visitor)
    }

    #[inline]
    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.de.deserialize_bytes(visitor)
    }

    forward_to_deserialize_any! {
        bool f32 f64 char str string unit unit_struct seq tuple tuple_struct map
        struct identifier ignored_any
    }
}

//////////////////////////////////////////////////////////////////////////////

/// Iterator that deserializes a stream into multiple JSON values.
///
/// A stream deserializer can be created from any JSON deserializer using the
/// `Deserializer::into_iter` method.
///
/// The data can consist of any JSON value. Values need to be a self-delineating value e.g.
/// arrays, objects, or strings, or be followed by whitespace or a self-delineating value.
///
/// ```rust
/// extern crate serde_json;
///
/// use serde_json::{Deserializer, Value};
///
/// fn main() {
///     let data = "{\"k\": 3}1\"cool\"\"stuff\" 3{}  [0, 1, 2]";
///
///     let stream = Deserializer::from_str(data).into_iter::<Value>();
///
///     for value in stream {
///         println!("{}", value.unwrap());
///     }
/// }
/// ```
pub struct StreamDeserializer<'de, R, T> {
    de: Deserializer<R>,
    offset: usize,
    output: PhantomData<T>,
    lifetime: PhantomData<&'de ()>,
}

impl<'de, R, T> StreamDeserializer<'de, R, T>
where
    R: read::Read<'de>,
    T: de::Deserialize<'de>,
{
    /// Create a JSON stream deserializer from one of the possible serde_json
    /// input sources.
    ///
    /// Typically it is more convenient to use one of these methods instead:
    ///
    ///   - Deserializer::from_str(...).into_iter()
    ///   - Deserializer::from_bytes(...).into_iter()
    ///   - Deserializer::from_reader(...).into_iter()
    pub fn new(read: R) -> Self {
        let offset = read.byte_offset();
        StreamDeserializer {
            de: Deserializer::new(read),
            offset: offset,
            output: PhantomData,
            lifetime: PhantomData,
        }
    }

    /// Returns the number of bytes so far deserialized into a successful `T`.
    ///
    /// If a stream deserializer returns an EOF error, new data can be joined to
    /// `old_data[stream.byte_offset()..]` to try again.
    ///
    /// ```rust
    /// let data = b"[0] [1] [";
    ///
    /// let de = serde_json::Deserializer::from_slice(data);
    /// let mut stream = de.into_iter::<Vec<i32>>();
    /// assert_eq!(0, stream.byte_offset());
    ///
    /// println!("{:?}", stream.next()); // [0]
    /// assert_eq!(3, stream.byte_offset());
    ///
    /// println!("{:?}", stream.next()); // [1]
    /// assert_eq!(7, stream.byte_offset());
    ///
    /// println!("{:?}", stream.next()); // error
    /// assert_eq!(8, stream.byte_offset());
    ///
    /// // If err.is_eof(), can join the remaining data to new data and continue.
    /// let remaining = &data[stream.byte_offset()..];
    /// ```
    ///
    /// *Note:* In the future this method may be changed to return the number of
    /// bytes so far deserialized into a successful T *or* syntactically valid
    /// JSON skipped over due to a type error. See [serde-rs/json#70] for an
    /// example illustrating this.
    ///
    /// [serde-rs/json#70]: https://github.com/serde-rs/json/issues/70
    pub fn byte_offset(&self) -> usize {
        self.offset
    }

    fn peek_end_of_value(&mut self) -> Result<()> {
        match try!(self.de.peek()) {
            Some(b' ') | Some(b'\n') | Some(b'\t') | Some(b'\r') |
            Some(b'"') | Some(b'[') | Some(b']') | Some(b'{') |
            Some(b'}') | Some(b',') | Some(b':') | None => Ok(()),
            Some(_) => {
                let pos = self.de.read.peek_position();
                Err(Error::syntax(ErrorCode::TrailingCharacters, pos.line, pos.column))
            },
        }
    }
}

impl<'de, R, T> Iterator for StreamDeserializer<'de, R, T>
where
    R: Read<'de>,
    T: de::Deserialize<'de>,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        // skip whitespaces, if any
        // this helps with trailing whitespaces, since whitespaces between
        // values are handled for us.
        match self.de.parse_whitespace() {
            Ok(None) => {
                self.offset = self.de.read.byte_offset();
                None
            }
            Ok(Some(b)) => {
                // If the value does not have a clear way to show the end of the value
                // (like numbers, null, true etc.) we have to look for whitespace or
                // the beginning of a self-delineated value.
                let self_delineated_value = match b {
                    b'[' | b'"' | b'{' => true,
                    _ => false,
                };
                self.offset = self.de.read.byte_offset();
                let result = de::Deserialize::deserialize(&mut self.de);

                Some(match result {
                    Ok(value) => {
                        self.offset = self.de.read.byte_offset();
                        if self_delineated_value {
                            Ok(value)
                        } else {
                            self.peek_end_of_value().map(|_| value)
                        }
                    }
                    Err(e) => Err(e)
                })
            }
            Err(e) => Some(Err(e))
        }
    }
}

//////////////////////////////////////////////////////////////////////////////

fn from_trait<'de, R, T>(read: R) -> Result<T>
where
    R: Read<'de>,
    T: de::Deserialize<'de>,
{
    let mut de = Deserializer::new(read);
    let value = try!(de::Deserialize::deserialize(&mut de));

    // Make sure the whole stream has been consumed.
    try!(de.end());
    Ok(value)
}

/// Deserialize an instance of type `T` from an IO stream of JSON.
///
/// # Errors
///
/// This conversion can fail if the structure of the input does not match the
/// structure expected by `T`, for example if `T` is a struct type but the input
/// contains something other than a JSON map. It can also fail if the structure
/// is correct but `T`'s implementation of `Deserialize` decides that something
/// is wrong with the data, for example required struct fields are missing from
/// the JSON map or some number is too big to fit in the expected primitive
/// type.
///
/// ```rust
/// #[macro_use]
/// extern crate serde_derive;
///
/// extern crate serde;
/// extern crate serde_json;
///
/// use std::error::Error;
/// use std::fs::File;
/// use std::path::Path;
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn read_user_from_file<P: AsRef<Path>>(path: P) -> Result<User, Box<Error>> {
///     // Open the file in read-only mode.
///     let file = File::open(path)?;
///
///     // Read the JSON contents of the file as an instance of `User`.
///     let u = serde_json::from_reader(file)?;
///
///     // Return the `User`.
///     Ok(u)
/// }
///
/// fn main() {
/// # }
/// # fn fake_main() {
///     let u = read_user_from_file("test.json").unwrap();
///     println!("{:#?}", u);
/// }
/// ```
pub fn from_reader<R, T>(rdr: R) -> Result<T>
where
    R: io::Read,
    T: de::DeserializeOwned,
{
    from_trait(read::IoRead::new(rdr))
}

/// Deserialize an instance of type `T` from bytes of JSON text.
///
/// # Errors
///
/// This conversion can fail if the structure of the input does not match the
/// structure expected by `T`, for example if `T` is a struct type but the input
/// contains something other than a JSON map. It can also fail if the structure
/// is correct but `T`'s implementation of `Deserialize` decides that something
/// is wrong with the data, for example required struct fields are missing from
/// the JSON map or some number is too big to fit in the expected primitive
/// type.
///
/// ```rust
/// #[macro_use]
/// extern crate serde_derive;
///
/// extern crate serde;
/// extern crate serde_json;
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn main() {
///     // The type of `j` is `&[u8]`
///     let j = b"{
///                 \"fingerprint\": \"0xF9BA143B95FF6D82\",
///                 \"location\": \"Menlo Park, CA\"
///               }";
///
///     let u: User = serde_json::from_slice(j).unwrap();
///     println!("{:#?}", u);
/// }
/// ```
pub fn from_slice<'a, T>(v: &'a [u8]) -> Result<T>
where
    T: de::Deserialize<'a>,
{
    from_trait(read::SliceRead::new(v))
}

/// Deserialize an instance of type `T` from a string of JSON text.
///
/// # Errors
///
/// This conversion can fail if the structure of the input does not match the
/// structure expected by `T`, for example if `T` is a struct type but the input
/// contains something other than a JSON map. It can also fail if the structure
/// is correct but `T`'s implementation of `Deserialize` decides that something
/// is wrong with the data, for example required struct fields are missing from
/// the JSON map or some number is too big to fit in the expected primitive
/// type.
///
/// ```rust
/// #[macro_use]
/// extern crate serde_derive;
///
/// extern crate serde;
/// extern crate serde_json;
///
/// #[derive(Deserialize, Debug)]
/// struct User {
///     fingerprint: String,
///     location: String,
/// }
///
/// fn main() {
///     // The type of `j` is `&str`
///     let j = "{
///                \"fingerprint\": \"0xF9BA143B95FF6D82\",
///                \"location\": \"Menlo Park, CA\"
///              }";
///
///     let u: User = serde_json::from_str(j).unwrap();
///     println!("{:#?}", u);
/// }
/// ```
pub fn from_str<'a, T>(s: &'a str) -> Result<T>
where
    T: de::Deserialize<'a>,
{
    from_trait(read::StrRead::new(s))
}
