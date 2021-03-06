//! Web3 helpers.

use std::marker::PhantomData;
use std::marker::Unpin;
use std::pin::Pin;

use crate::{error, rpc};
use futures::{
    task::{Context, Poll},
    Future, FutureExt,
};
use serde;
use serde_json;

/// Value-decoder future.
/// Takes any type which is deserializable from rpc::Value and a future which yields that
/// type, and yields the deserialized value
#[derive(Debug)]
pub struct CallFuture<T, F> {
    inner: F,
    _marker: PhantomData<T>,
}

impl<T, F> CallFuture<T, F> {
    /// Create a new CallFuture wrapping the inner future.
    pub fn new(inner: F) -> Self {
        CallFuture {
            inner,
            _marker: PhantomData,
        }
    }
}

impl<T, F> Future for CallFuture<T, F>
where
    T: serde::de::DeserializeOwned + Unpin,
    F: Future<Output = error::Result<rpc::Value>> + Unpin,
{
    type Output = error::Result<T>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let x = ready!(self.inner.poll_unpin(ctx));
        Poll::Ready(x.and_then(|x| serde_json::from_value(x).map_err(Into::into)))
    }
}

/// Serialize a type. Panics if the type is returns error during serialization.
pub fn serialize<T: serde::Serialize>(t: &T) -> rpc::Value {
    serde_json::to_value(t).expect("Types never fail to serialize.")
}

/// Serializes a request to string. Panics if the type returns error during serialization.
pub fn to_string<T: serde::Serialize>(request: &T) -> String {
    serde_json::to_string(&request).expect("String serialization never fails.")
}

/// Build a JSON-RPC request.
pub fn build_request(id: usize, method: &str, params: Vec<rpc::Value>) -> rpc::Call {
    rpc::Call::MethodCall(rpc::MethodCall {
        jsonrpc: Some(rpc::Version::V2),
        method: method.into(),
        params: rpc::Params::Array(params),
        id: rpc::Id::Num(id as u64),
    })
}

/// Parse bytes slice into JSON-RPC response.
pub fn to_response_from_slice(response: &[u8]) -> error::Result<rpc::Response> {
    serde_json::from_slice(response).map_err(|e| error::Error::InvalidResponse(format!("{:?}", e)))
}

/// Parse bytes slice into JSON-RPC notification.
pub fn to_notification_from_slice(notification: &[u8]) -> error::Result<rpc::Notification> {
    serde_json::from_slice(notification).map_err(|e| error::Error::InvalidResponse(format!("{:?}", e)))
}

/// Parse a Vec of `rpc::Output` into `Result`.
pub fn to_results_from_outputs(outputs: Vec<rpc::Output>) -> error::Result<Vec<error::Result<rpc::Value>>> {
    Ok(outputs.into_iter().map(to_result_from_output).collect())
}

/// Parse `rpc::Output` into `Result`.
pub fn to_result_from_output(output: rpc::Output) -> error::Result<rpc::Value> {
    match output {
        rpc::Output::Success(success) => Ok(success.result),
        rpc::Output::Failure(failure) => Err(error::Error::Rpc(failure.error)),
    }
}

#[macro_use]
#[cfg(test)]
pub mod tests {
    use crate::error::{self, Error};
    use crate::rpc;
    use crate::{RequestId, Transport};
    use futures::future;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::marker::Unpin;
    use std::rc::Rc;

    type Result<T> = Box<dyn futures::Future<Output = error::Result<T>> + Send + Unpin>;

    #[derive(Debug, Default, Clone)]
    pub struct TestTransport {
        asserted: usize,
        requests: Rc<RefCell<Vec<(String, Vec<rpc::Value>)>>>,
        responses: Rc<RefCell<VecDeque<rpc::Value>>>,
    }

    impl Transport for TestTransport {
        type Out = Result<rpc::Value>;

        fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (RequestId, rpc::Call) {
            let request = super::build_request(1, method, params.clone());
            self.requests.borrow_mut().push((method.into(), params));
            (self.requests.borrow().len(), request)
        }

        fn send(&self, id: RequestId, request: rpc::Call) -> Result<rpc::Value> {
            Box::new(future::ready(match self.responses.borrow_mut().pop_front() {
                Some(response) => Ok(response),
                None => {
                    println!("Unexpected request (id: {:?}): {:?}", id, request);
                    Err(Error::Unreachable)
                }
            }))
        }
    }

    impl TestTransport {
        pub fn set_response(&mut self, value: rpc::Value) {
            *self.responses.borrow_mut() = vec![value].into();
        }

        pub fn add_response(&mut self, value: rpc::Value) {
            self.responses.borrow_mut().push_back(value);
        }

        pub fn assert_request(&mut self, method: &str, params: &[String]) {
            let idx = self.asserted;
            self.asserted += 1;

            let (m, p) = self.requests.borrow().get(idx).expect("Expected result.").clone();
            assert_eq!(&m, method);
            let p: Vec<String> = p.into_iter().map(|p| serde_json::to_string(&p).unwrap()).collect();
            assert_eq!(p, params);
        }

        pub fn assert_no_more_requests(&self) {
            let requests = self.requests.borrow();
            assert_eq!(
                self.asserted,
                requests.len(),
                "Expected no more requests, got: {:?}",
                &requests[self.asserted..]
            );
        }
    }

    macro_rules! rpc_test {
    // With parameters
    (
      $namespace: ident : $name: ident : $test_name: ident  $(, $param: expr)+ => $method: expr,  $results: expr;
      $returned: expr => $expected: expr
    ) => {
      #[test]
      fn $test_name() {
        // given
        let mut transport = $crate::helpers::tests::TestTransport::default();
        transport.set_response($returned);
        let result = {
          let eth = $namespace::new(&transport);

          // when
          eth.$name($($param.into(), )+)
        };

        // then
        transport.assert_request($method, &$results.into_iter().map(Into::into).collect::<Vec<_>>());
        transport.assert_no_more_requests();
        let result = futures::executor::block_on(result);
        assert_eq!(result, Ok($expected.into()));
      }
    };
    // With parameters (implicit test name)
    (
      $namespace: ident : $name: ident $(, $param: expr)+ => $method: expr,  $results: expr;
      $returned: expr => $expected: expr
    ) => {
      rpc_test! (
        $namespace : $name : $name $(, $param)+ => $method, $results;
        $returned => $expected
      );
    };

    // No params entry point (explicit name)
    (
      $namespace: ident: $name: ident: $test_name: ident => $method: expr;
      $returned: expr => $expected: expr
    ) => {
      #[test]
      fn $test_name() {
        // given
        let mut transport = $crate::helpers::tests::TestTransport::default();
        transport.set_response($returned);
        let result = {
          let eth = $namespace::new(&transport);

          // when
          eth.$name()
        };

        // then
        transport.assert_request($method, &[]);
        transport.assert_no_more_requests();
        let result = futures::executor::block_on(result);
        assert_eq!(result, Ok($expected.into()));
      }
    };

    // No params entry point
    (
      $namespace: ident: $name: ident => $method: expr;
      $returned: expr => $expected: expr
    ) => {
      rpc_test! (
        $namespace: $name: $name => $method;
        $returned => $expected
      );
    }
  }
}
